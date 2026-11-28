// SPDX-License-Identifier: MIT
//! End-to-end tests for `taskfast fee retry` (the voucher-carrying and
//! wallet-signing successor of `task retry-fee`) and for the old command's
//! redirect. wiremock serves the API; the chain is the in-memory
//! [`MockTempoRpc`], behind the deployment's proxy or a `--rpc-url`.

use std::sync::Arc;

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use serde_json::{json, Value};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::tempo_session::{register_mock_rpc, MockTempoRpc};

use taskfast_cli::cmd::fee::{run, Command, RetryArgs};
use taskfast_cli::cmd::task::{self, IdOnlyArgs};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const TASK_ID: &str = "00000000-0000-0000-0000-0000000000aa";
const VOUCHER: &str = "0xdeadbeef";
const TOKEN_ADDR: &str = "0x20c0000000000000000000000000000000000000";
const RECIPIENT: &str = "0x9999999999999999999999999999999999999999";
const AMOUNT: &str = "250000";
const MEMO_TEXT: &str =
    "submission_fee_retry: task 00000000-0000-0000-0000-0000000000aa replaces 123";
const MEMO_HASH: &str = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";

fn ctx_for(server: &MockServer) -> Ctx {
    Ctx {
        api_key: Some("test-key".into()),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run: false,
        quiet: true,
        ..Default::default()
    }
}

fn envelope_value(env: &Envelope) -> serde_json::Value {
    serde_json::to_value(env).expect("serialize envelope")
}

fn retry_with_voucher(voucher: &str) -> Command {
    Command::Retry(RetryArgs {
        task_id: TASK_ID.into(),
        submission_fee_voucher: Some(voucher.into()),
        wallet_address: None,
        keystore: None,
        wallet_password_file: None,
        rpc_url: None,
    })
}

fn retry_wallet(
    keystore: &str,
    pw_file: &str,
    wallet_addr: &str,
    rpc_url: Option<String>,
) -> Command {
    Command::Retry(RetryArgs {
        task_id: TASK_ID.into(),
        submission_fee_voucher: None,
        wallet_address: Some(wallet_addr.into()),
        keystore: Some(keystore.into()),
        wallet_password_file: Some(std::path::PathBuf::from(pw_file)),
        rpc_url,
    })
}

fn sample_calldata() -> String {
    use alloy_sol_types::SolCall;
    let call = taskfast_cli::cmd::fee::transferWithMemoCall {
        to: RECIPIENT.parse().unwrap(),
        amount: alloy_primitives::U256::from(250000),
        memo_hash: MEMO_HASH.parse().unwrap(),
    };
    format!("0x{}", hex::encode(call.abi_encode()))
}

struct TestWallet {
    _tmp: tempfile::TempDir,
    keystore_path: String,
    password_path: String,
    address: String,
}

fn create_test_wallet() -> TestWallet {
    let signer = PrivateKeySigner::random();
    let address = format!("{:#x}", signer.address());
    let tmp = tempfile::tempdir().expect("tempdir");
    let keystore_path = tmp.path().join("wallet.json");
    taskfast_agent::keystore::save_signer(&signer, &keystore_path, "pw").expect("save keystore");
    let password_path = tmp.path().join("pw");
    std::fs::write(&password_path, b"pw").expect("write pw");
    TestWallet {
        _tmp: tmp,
        keystore_path: keystore_path.to_str().unwrap().to_string(),
        password_path: password_path.to_str().unwrap().to_string(),
        address,
    }
}

/// A fresh `--rpc-url` served by an in-memory testnet node.
fn testnet_node() -> (String, Arc<MockTempoRpc>) {
    let url = format!("http://node-{}/rpc", uuid::Uuid::new_v4().simple());
    let node = Arc::new(MockTempoRpc::new());
    node.set_chain_id(42431);
    register_mock_rpc(&url, node.clone());
    (url, node)
}

async fn mock_task_status(server: &MockServer, next_action: Option<&str>, message: Option<&str>) {
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": TASK_ID,
            "title": "retry test",
            "status": "blocked_on_submission_fee_debt",
            "submission_fee_status": if next_action.is_some() { "failed" } else { "pending_confirmation" },
            "actionable": false,
            "blocked_reason": "submission_fee_debt",
            "next_action": next_action,
            "message": message,
        })))
        .mount(server)
        .await;
}

async fn mock_payment_data(server: &MockServer, token_address: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}/retry-fee")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token_address": token_address,
            "recipient": RECIPIENT,
            "amount": AMOUNT,
            "memo_text": MEMO_TEXT,
            "memo_hash": MEMO_HASH,
            "payload_to_sign": sample_calldata(),
        })))
        .mount(server)
        .await;
}

async fn error_with_status(status: u16, code: &str) -> CmdError {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/retry-fee")))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({
            "error": code,
            "message": "server says no",
        })))
        .mount(&server)
        .await;
    run(&ctx_for(&server), retry_with_voucher(VOUCHER))
        .await
        .expect_err("server refusal must surface")
}

#[tokio::test]
async fn fee_retry_sends_voucher_unchanged_in_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/retry-fee")))
        .and(body_json(json!({ "submission_fee_voucher": VOUCHER })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": TASK_ID,
            "status": "pending_evaluation",
            "message": "Submission fee charge re-attempted",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let env = run(&ctx_for(&server), retry_with_voucher(VOUCHER))
        .await
        .expect("ok");
    let v = envelope_value(&env);
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["status"], "pending_evaluation");
}

#[tokio::test]
async fn fee_retry_signs_and_broadcasts_when_voucher_omitted() {
    let api_server = MockServer::start().await;
    let (rpc_url, node) = testnet_node();
    let wallet = create_test_wallet();

    mock_task_status(&api_server, Some("retry_submission_fee"), None).await;
    mock_payment_data(&api_server, TOKEN_ADDR).await;

    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/retry-fee")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": TASK_ID,
            "status": "pending_evaluation",
            "message": "Submission fee charge re-attempted",
        })))
        .expect(1)
        .mount(&api_server)
        .await;

    let cmd = retry_wallet(
        &wallet.keystore_path,
        &wallet.password_path,
        &wallet.address,
        Some(rpc_url),
    );
    let mut ctx = ctx_for(&api_server);
    ctx.allow_custom_endpoints = true;

    let env = run(&ctx, cmd).await.expect("fee retry succeeded");
    let v = envelope_value(&env);
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["status"], "pending_evaluation");

    // One broadcast: the fee token is the target (NOT the recipient), the
    // calldata is the server's payload verbatim, and it is signed for the
    // node's chain. Its hash is the voucher the server received.
    let sent = node.sent_transactions();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].0,
        TOKEN_ADDR.parse::<Address>().unwrap(),
        "target of transaction must be allowlisted fee token, NOT recipient"
    );
    let expected_calldata = hex::decode(sample_calldata().strip_prefix("0x").unwrap()).unwrap();
    assert_eq!(
        sent[0].1.as_ref(),
        expected_calldata.as_slice(),
        "calldata must match payload_to_sign exactly"
    );
    assert_eq!(node.broadcast_chain_ids(), [Some(42431)]);
    let tx_hash_hex = format!("{:#x}", node.tx_hashes()[0]);
    assert_eq!(v["data"]["submission_fee_voucher"], tx_hash_hex);
    let retries: Vec<Value> = api_server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/retry-fee"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0]["submission_fee_voucher"], tx_hash_hex);
}

#[tokio::test]
async fn fee_retry_sends_nothing_when_server_reports_retry_not_needed() {
    let api_server = MockServer::start().await;
    let (rpc_url, node) = testnet_node();
    let wallet = create_test_wallet();

    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}/retry-fee")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "retry_not_needed",
            "message": "Task is not awaiting fee retry",
        })))
        .mount(&api_server)
        .await;

    let cmd = retry_wallet(
        &wallet.keystore_path,
        &wallet.password_path,
        &wallet.address,
        Some(rpc_url),
    );
    let mut ctx = ctx_for(&api_server);
    ctx.allow_custom_endpoints = true;

    let err = run(&ctx, cmd).await.expect_err("should refuse");
    match err {
        CmdError::Validation { code, message } => {
            assert_eq!(code, "retry_not_needed");
            assert!(
                message.contains("Task is not awaiting fee retry"),
                "expected server reason, got {message}"
            );
        }
        other => panic!("expected Validation error, got {other:?}"),
    }

    assert!(
        node.request_count() == 0,
        "zero node requests must be made when server returns retry_not_needed"
    );
}

async fn mount_network_config_mock(server: &MockServer) -> Arc<MockTempoRpc> {
    mount_network_config_mock_on(server, 42431).await
}

/// Advertise a `testnet` entry on `chain_id`, served by a node on the same
/// chain registered under the proxy URL.
async fn mount_network_config_mock_on(server: &MockServer, chain_id: u64) -> Arc<MockTempoRpc> {
    let uri = server.uri();
    let payload = json!({
        "networks": {
            "testnet": {
                "chain_id": chain_id,
                "rpc_url": format!("{uri}/rpc/testnet"),
                "wss_url": "wss://testnet.example.invalid",
                "explorer_url": "https://explorer-testnet.example.invalid",
                "default_stablecoin": "PathUSD"
            }
        }
    });
    Mock::given(method("GET"))
        .and(path("/config/network"))
        .respond_with(ResponseTemplate::new(200).set_body_json(payload))
        .mount(server)
        .await;
    let node = Arc::new(MockTempoRpc::new());
    node.set_chain_id(chain_id);
    register_mock_rpc(&format!("{uri}/rpc/testnet"), node.clone());
    node
}

/// Through the proxy the node must be on the entry's chain, so an unknown chain
/// there is a `chain_id_mismatch`; the allowlist guard covers a deployment
/// that advertises the unknown chain itself.
#[tokio::test]
async fn fee_retry_guards_unknown_chain_id() {
    let api_server = MockServer::start().await;
    let wallet = create_test_wallet();

    let node = mount_network_config_mock_on(&api_server, 999_999).await;
    mock_task_status(&api_server, Some("retry_submission_fee"), None).await;
    mock_payment_data(&api_server, TOKEN_ADDR).await;

    let cmd = retry_wallet(
        &wallet.keystore_path,
        &wallet.password_path,
        &wallet.address,
        None,
    );
    let mut ctx = ctx_for(&api_server);
    ctx.environment = Environment::Local;
    ctx.allow_custom_endpoints = false;

    let err = run(&ctx, cmd).await.expect_err("unknown chain should fail");
    match err {
        CmdError::Validation { code, message } => {
            assert_eq!(code, "unknown_chain");
            assert!(message.contains("999999"), "{message}");
        }
        other => panic!("expected Validation unknown_chain, got {other:?}"),
    }
    assert!(node.sent_transactions().is_empty(), "never signed");
}

/// PathUSD has the same address on both L1s, so a proxy misrouted to the
/// other network's node passes the allowlist; the proxy route pins the
/// entry's chain id so it fails before anything is signed.
#[tokio::test]
async fn fee_retry_refuses_a_proxy_node_on_another_chain() {
    let api_server = MockServer::start().await;
    let wallet = create_test_wallet();

    let node = mount_network_config_mock(&api_server).await;
    node.set_chain_id(4217);
    mock_task_status(&api_server, Some("retry_submission_fee"), None).await;
    mock_payment_data(&api_server, TOKEN_ADDR).await;

    let cmd = retry_wallet(
        &wallet.keystore_path,
        &wallet.password_path,
        &wallet.address,
        None,
    );
    let mut ctx = ctx_for(&api_server);
    ctx.environment = Environment::Local;

    let err = run(&ctx, cmd)
        .await
        .expect_err("mainnet node behind a testnet proxy");
    assert!(
        matches!(&err, CmdError::Validation { code, .. } if code == "chain_id_mismatch"),
        "{err:?}"
    );
    assert!(node.sent_transactions().is_empty(), "never signed");
}

#[tokio::test]
async fn fee_retry_guards_disallowed_fee_token() {
    let api_server = MockServer::start().await;
    let (rpc_url, node) = testnet_node();
    let wallet = create_test_wallet();

    mock_task_status(&api_server, Some("retry_submission_fee"), None).await;
    // An arbitrary token address not in the PathUSD allowlist
    let bad_token = "0x1111111111111111111111111111111111111111";
    mock_payment_data(&api_server, bad_token).await;

    let cmd = retry_wallet(
        &wallet.keystore_path,
        &wallet.password_path,
        &wallet.address,
        Some(rpc_url),
    );
    let mut ctx = ctx_for(&api_server);
    ctx.allow_custom_endpoints = true;

    let err = run(&ctx, cmd).await.expect_err("bad token should fail");
    match err {
        CmdError::Validation { code, message } => {
            assert_eq!(code, "fee_token_not_allowed");
            assert!(message.contains(bad_token), "{message}");
        }
        other => panic!("expected fee_token_not_allowed, got {other:?}"),
    }
    assert!(node.sent_transactions().is_empty(), "never signed");
}

#[tokio::test]
async fn fee_retry_dry_run_prints_payment_data_without_chain_or_submit() {
    let api_server = MockServer::start().await;
    let (rpc_url, node) = testnet_node();
    let wallet = create_test_wallet();

    mock_task_status(&api_server, Some("retry_submission_fee"), None).await;
    mock_payment_data(&api_server, TOKEN_ADDR).await;

    let cmd = retry_wallet(
        &wallet.keystore_path,
        &wallet.password_path,
        &wallet.address,
        Some(rpc_url),
    );
    let mut ctx = ctx_for(&api_server);
    ctx.dry_run = true;
    ctx.allow_custom_endpoints = true;

    let env = run(&ctx, cmd).await.expect("dry-run succeeds");
    let v = envelope_value(&env);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["token_address"], TOKEN_ADDR);
    assert!(v["data"].get("token").is_none());
    assert_eq!(v["data"]["recipient"], RECIPIENT);
    assert_eq!(v["data"]["amount"], AMOUNT);
    assert_eq!(v["data"]["memo_text"], MEMO_TEXT);
    assert_eq!(v["data"]["memo_hash"], MEMO_HASH);
    assert!(v["data"].get("memo").is_none());

    assert_eq!(node.request_count(), 0, "no chain or submit");
}

#[tokio::test]
async fn fee_retry_server_500_after_broadcast_returns_typed_server_error() {
    let api_server = MockServer::start().await;
    let (rpc_url, node) = testnet_node();
    let wallet = create_test_wallet();

    mock_payment_data(&api_server, TOKEN_ADDR).await;

    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/retry-fee")))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "internal_server_error",
            "message": "database exploded",
        })))
        .mount(&api_server)
        .await;

    let cmd = retry_wallet(
        &wallet.keystore_path,
        &wallet.password_path,
        &wallet.address,
        Some(rpc_url),
    );
    let mut ctx = ctx_for(&api_server);
    ctx.allow_custom_endpoints = true;

    let err = run(&ctx, cmd)
        .await
        .expect_err("500 after broadcast must error");
    assert_eq!(node.sent_transactions().len(), 1, "the broadcast happened");
    match err {
        CmdError::Server(msg) => {
            assert!(msg.contains("database exploded"));
        }
        other => panic!("expected Server error, got {other:?}"),
    }
}

#[tokio::test]
async fn fee_retry_post_broadcast_server_409_preserves_error_code() {
    let api_server = MockServer::start().await;
    let (rpc_url, node) = testnet_node();
    let wallet = create_test_wallet();

    mock_payment_data(&api_server, TOKEN_ADDR).await;

    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/retry-fee")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "retry_in_progress",
            "message": "another retry holds the task",
        })))
        .mount(&api_server)
        .await;

    let cmd = retry_wallet(
        &wallet.keystore_path,
        &wallet.password_path,
        &wallet.address,
        Some(rpc_url),
    );
    let mut ctx = ctx_for(&api_server);
    ctx.allow_custom_endpoints = true;

    let err = run(&ctx, cmd).await.expect_err("409 must fail");
    assert_eq!(node.sent_transactions().len(), 1, "the broadcast happened");
    match err {
        CmdError::Validation { code, message } => {
            assert_eq!(code, "retry_in_progress");
            assert!(message.contains("another retry holds the task"));
        }
        other => panic!("expected Validation retry_in_progress, got {other:?}"),
    }
}

#[tokio::test]
async fn fee_retry_reuses_pending_broadcast_tx_on_server_failure() {
    let api_server = MockServer::start().await;
    let (rpc_url, node) = testnet_node();
    let wallet = create_test_wallet();

    mock_payment_data(&api_server, TOKEN_ADDR).await;

    // First attempt: POST /retry-fee returns 500
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/retry-fee")))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "internal_server_error",
            "message": "database exploded",
        })))
        .up_to_n_times(1)
        .mount(&api_server)
        .await;

    let tmp_config = tempfile::tempdir().expect("tempdir");
    let config_path = tmp_config.path().join("config.json");

    let cmd = retry_wallet(
        &wallet.keystore_path,
        &wallet.password_path,
        &wallet.address,
        Some(rpc_url.clone()),
    );
    let mut ctx = ctx_for(&api_server);
    ctx.config_path = config_path.clone();
    ctx.allow_custom_endpoints = true;

    let err = run(&ctx, cmd).await.expect_err("first run must fail");
    assert!(matches!(err, CmdError::Server(msg) if msg.contains("database exploded")));

    // Exactly one broadcast so far; its hash is the pending voucher.
    assert_eq!(node.sent_transactions().len(), 1);
    let tx_hash_hex = format!("{:#x}", node.tx_hashes()[0]);

    // Second attempt: server returns 200
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/retry-fee")))
        .and(body_json(json!({ "submission_fee_voucher": tx_hash_hex })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": TASK_ID,
            "status": "pending_evaluation",
            "message": "Submission fee charge re-attempted",
        })))
        .mount(&api_server)
        .await;

    let cmd2 = retry_wallet(
        &wallet.keystore_path,
        &wallet.password_path,
        &wallet.address,
        Some(rpc_url),
    );
    let env = run(&ctx, cmd2)
        .await
        .expect("second run must reuse tx and succeed");
    let v = envelope_value(&env);
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["submission_fee_voucher"], tx_hash_hex);

    // No second broadcast: the pending voucher was reused as-is.
    assert_eq!(
        node.sent_transactions().len(),
        1,
        "no second broadcast allowed!"
    );

    // Verify pending tx was cleared on success
    let store = ctx.state_store();
    let task_uuid = uuid::Uuid::parse_str(TASK_ID).unwrap();
    assert_eq!(store.get_pending_fee_retry(&task_uuid), None);
}

#[tokio::test]
async fn fee_retry_guards_calldata_recipient_mismatch() {
    let api_server = MockServer::start().await;
    let (rpc_url, node) = testnet_node();
    let wallet = create_test_wallet();

    use alloy_sol_types::SolCall;
    let attacker_recipient = "0x8888888888888888888888888888888888888888";
    let bad_call = taskfast_cli::cmd::fee::transferWithMemoCall {
        to: attacker_recipient.parse().unwrap(),
        amount: alloy_primitives::U256::from(250000),
        memo_hash: MEMO_HASH.parse().unwrap(),
    };
    let bad_calldata = format!("0x{}", hex::encode(bad_call.abi_encode()));

    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}/retry-fee")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token_address": TOKEN_ADDR,
            "recipient": RECIPIENT,
            "amount": AMOUNT,
            "memo_text": MEMO_TEXT,
            "memo_hash": MEMO_HASH,
            "payload_to_sign": bad_calldata,
        })))
        .mount(&api_server)
        .await;

    let cmd = retry_wallet(
        &wallet.keystore_path,
        &wallet.password_path,
        &wallet.address,
        Some(rpc_url),
    );
    let mut ctx = ctx_for(&api_server);
    ctx.allow_custom_endpoints = true;

    let err = run(&ctx, cmd)
        .await
        .expect_err("calldata mismatch must fail");
    match err {
        CmdError::Validation { code, message } => {
            assert_eq!(code, "calldata_mismatch");
            assert!(message.contains("recipient"), "{message}");
        }
        other => panic!("expected calldata_mismatch, got {other:?}"),
    }

    assert_eq!(node.request_count(), 0, "never touched the chain");
}

#[tokio::test]
async fn fee_retry_empty_voucher_is_usage_error_without_http() {
    let server = MockServer::start().await;
    let err = run(&ctx_for(&server), retry_with_voucher("  "))
        .await
        .expect_err("empty");
    match err {
        CmdError::Usage(m) => assert!(m.contains("freshly signed"), "{m}"),
        other => panic!("expected Usage, got {other:?}"),
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn fee_retry_non_hex_voucher_is_usage_error_without_http() {
    let server = MockServer::start().await;
    let err = run(&ctx_for(&server), retry_with_voucher("not-hex"))
        .await
        .expect_err("non-hex");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn fee_retry_dry_run_with_voucher_prints_task_and_voucher_without_http() {
    let server = MockServer::start().await;
    let mut ctx = ctx_for(&server);
    ctx.dry_run = true;
    let env = run(&ctx, retry_with_voucher(VOUCHER))
        .await
        .expect("dry-run");
    let v = envelope_value(&env);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["submission_fee_voucher"], VOUCHER);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn fee_retry_400_invalid_voucher_keeps_stable_code() {
    match error_with_status(400, "invalid_submission_fee_voucher").await {
        CmdError::Validation { code, .. } => assert_eq!(code, "invalid_submission_fee_voucher"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn fee_retry_409_retry_not_needed_keeps_stable_code() {
    match error_with_status(409, "retry_not_needed").await {
        CmdError::Validation { code, .. } => assert_eq!(code, "retry_not_needed"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn fee_retry_409_retry_in_progress_keeps_stable_code() {
    match error_with_status(409, "retry_in_progress").await {
        CmdError::Validation { code, .. } => assert_eq!(code, "retry_in_progress"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn fee_retry_409_fee_executed_unmatched_keeps_stable_code() {
    match error_with_status(409, "fee_executed_unmatched").await {
        CmdError::Validation { code, .. } => assert_eq!(code, "fee_executed_unmatched"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn fee_retry_403_is_auth() {
    assert!(matches!(
        error_with_status(403, "forbidden").await,
        CmdError::Auth(_)
    ));
}

#[tokio::test]
async fn fee_retry_bad_uuid_is_usage_error_without_http() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server),
        Command::Retry(RetryArgs {
            task_id: "bad".into(),
            submission_fee_voucher: Some(VOUCHER.into()),
            wallet_address: None,
            keystore: None,
            wallet_password_file: None,
            rpc_url: None,
        }),
    )
    .await
    .expect_err("bad uuid");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn old_task_retry_fee_redirects_without_http() {
    let server = MockServer::start().await;
    let err = task::run(
        &ctx_for(&server),
        task::Command::RetryFee(IdOnlyArgs { id: TASK_ID.into() }),
    )
    .await
    .expect_err("redirect");
    match err {
        CmdError::Usage(m) => {
            assert!(
                m.contains(&format!("`taskfast fee retry {TASK_ID}`")),
                "{m}"
            );
        }
        other => panic!("expected Usage, got {other:?}"),
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn old_task_retry_fee_dry_run_prints_the_same_redirect() {
    let server = MockServer::start().await;
    let mut ctx = ctx_for(&server);
    ctx.dry_run = true;
    let err = task::run(
        &ctx,
        task::Command::RetryFee(IdOnlyArgs { id: TASK_ID.into() }),
    )
    .await
    .expect_err("redirect even in dry-run");
    assert!(
        matches!(&err, CmdError::Usage(m) if m.contains("taskfast fee retry")),
        "got {err:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn fee_retry_refuses_unwritable_state_dir_before_broadcasting() {
    let api_server = MockServer::start().await;
    let (rpc_url, node) = testnet_node();
    let wallet = create_test_wallet();
    mock_task_status(&api_server, Some("retry_submission_fee"), None).await;
    mock_payment_data(&api_server, TOKEN_ADDR).await;

    let tmp = tempfile::TempDir::new().unwrap();
    let shared = tmp.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    let link = tmp.path().join(".taskfast");
    std::os::unix::fs::symlink(&shared, &link).unwrap();

    let cmd = retry_wallet(
        &wallet.keystore_path,
        &wallet.password_path,
        &wallet.address,
        Some(rpc_url),
    );
    let mut ctx = ctx_for(&api_server);
    ctx.allow_custom_endpoints = true;
    ctx.config_path = link.join("config.json");

    let err = run(&ctx, cmd).await.expect_err("state dir must be refused");
    assert!(err.to_string().contains("symlink"), "{err}");
    assert!(
        node.sent_transactions().is_empty(),
        "nothing may be broadcast"
    );
}

#[tokio::test]
async fn fee_retry_directory_at_retry_file_prevents_broadcast() {
    let api_server = MockServer::start().await;
    let (rpc_url, node) = testnet_node();
    let wallet = create_test_wallet();
    mock_task_status(&api_server, Some("retry_submission_fee"), None).await;
    mock_payment_data(&api_server, TOKEN_ADDR).await;

    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join(".taskfast");
    std::fs::create_dir_all(dir.join("fee_retries.json")).unwrap();

    let cmd = retry_wallet(
        &wallet.keystore_path,
        &wallet.password_path,
        &wallet.address,
        Some(rpc_url),
    );
    let mut ctx = ctx_for(&api_server);
    ctx.allow_custom_endpoints = true;
    ctx.config_path = dir.join("config.json");

    let err = run(&ctx, cmd)
        .await
        .expect_err("directory destination must be refused");
    assert!(err.to_string().contains("directory"), "{err}");
    assert!(
        node.sent_transactions().is_empty(),
        "nothing may be broadcast"
    );
}
