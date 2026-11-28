// SPDX-License-Identifier: MIT
//! Live signing flows driven through `TempoSession`'s in-memory RPC adapter.
//!
//! The TaskFast API is a wiremock server; the Tempo node is a
//! [`MockTempoRpc`] registered under the deployment's advertised proxy URL, so
//! every command runs its real path (config lookup → same-host guard → node
//! chain-id guard → wallet unlock → sign → broadcast → receipt) without
//! per-command JSON-RPC scaffolding.

use std::path::PathBuf;
use std::sync::Arc;

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use serde_json::{json, Value};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use alloy_sol_types::SolCall;
use taskfast_agent::chain::{
    dispute_timeout_calldata, disputed_at_calldata, get_escrow_calldata, timeout_resolve_calldata,
    TaskBond, TaskEscrow, IERC20,
};
use taskfast_cli::cmd::bond::{BondStakeSource, Command as BondCommand, PostArgs};
use taskfast_cli::cmd::escalate::{
    Command as EscalateCommand, SignArgs as EscalateSignArgs, TimeoutResolveArgs,
};
use taskfast_cli::cmd::escrow::{Command as EscrowCommand, SignArgs, SubmitWorkArgs};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::tempo_session::{register_mock_rpc, MockTempoRpc};
use taskfast_cli::{Envelope, Environment};

const BID_ID: &str = "00000000-0000-0000-0000-0000000000bb";
const TASK_ID: &str = "00000000-0000-0000-0000-0000000000aa";
const BOND_TASK_ID: &str = "00112233-4455-6677-8899-aabbccddeeff";
const TASK_ESCROW: &str = "0x0000000000000000000000000000000000000001";
const TOKEN_ADDR: &str = "0x0000000000000000000000000000000000000002";
const TASK_BOND: &str = "0x31de2fd7d1d4bfcfb3d2b4bfc30f6b46f2b55db2";
const WORKER_ADDR: &str = "0x0000000000000000000000000000000000000003";
const PLATFORM_WALLET: &str = "0x0000000000000000000000000000000000000004";
const ARBITRATOR_ADDR: &str = "0x0000000000000000000000000000000000000005";
const ESCROW_ID: &str = "0xabababababababababababababababababababababababababababababababab";
const CHAIN_ID: u64 = 42_431;
const ZONE_CHAIN_ID: u64 = 42_432;
const MAINNET_CHAIN_ID: u64 = taskfast_chains::tempo::TEMPO_MAINNET_CHAIN_ID;
/// A mainnet Tempo Zone: its own chain id, on neither L1.
const MAINNET_ZONE_CHAIN_ID: u64 = 421_700_005;
const FAR_FUTURE: i64 = 4_102_444_800; // 2100-01-01
const MIN_LIFETIME: i64 = 10_368_000;
const DISPUTED_AT: u64 = 1_800_000_000;
const TIMEOUT: u64 = 10_368_000;

fn ctx_for(server: &MockServer) -> Ctx {
    Ctx {
        api_key: Some("k".into()),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path: PathBuf::from("/dev/null"),
        quiet: true,
        allow_custom_endpoints: true,
        ..Default::default()
    }
}

/// A `--env prod` run against the same mocked deployment.
fn prod_ctx_for(server: &MockServer) -> Ctx {
    Ctx {
        environment: Environment::Prod,
        ..ctx_for(server)
    }
}

fn val(env: &Envelope) -> Value {
    serde_json::to_value(env).expect("serialize envelope")
}

struct Keys {
    _tmp: tempfile::TempDir,
    keystore: PathBuf,
    password: PathBuf,
    address: Address,
}

fn fresh_keys() -> Keys {
    let signer = PrivateKeySigner::random();
    let tmp = tempfile::tempdir().unwrap();
    let keystore = tmp.path().join("wallet.json");
    taskfast_agent::keystore::save_signer(&signer, &keystore, "pw").unwrap();
    let password = tmp.path().join("pw");
    std::fs::write(&password, b"pw").unwrap();
    Keys {
        address: signer.address(),
        _tmp: tmp,
        keystore,
        password,
    }
}

fn word(n: u64) -> String {
    format!("{n:064x}")
}

fn sel(calldata: &[u8]) -> [u8; 4] {
    calldata[..4].try_into().unwrap()
}

fn hex_sel(selector: [u8; 4]) -> String {
    hex::encode(selector)
}

/// Mount `GET /config/network` advertising `testnet` (and optionally a zone
/// venue), then register an in-memory node under the testnet proxy URL.
async fn chain(server: &MockServer) -> Arc<MockTempoRpc> {
    mount_network(server, CHAIN_ID, None).await;
    node(&format!("{}/rpc/testnet", server.uri()))
}

async fn mount_network(server: &MockServer, testnet_chain_id: u64, zone: Option<u64>) {
    let mut networks = json!({ "testnet": entry(server, "testnet", testnet_chain_id) });
    if let Some(zone_chain_id) = zone {
        networks["tempo-zone-moderato"] = entry(server, "tempo-zone-moderato", zone_chain_id);
    }
    mount_networks(server, networks).await;
}

async fn mount_networks(server: &MockServer, networks: Value) {
    Mock::given(method("GET"))
        .and(path("/config/network"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "networks": networks })))
        .mount(server)
        .await;
}

fn entry(server: &MockServer, name: &str, chain_id: u64) -> Value {
    json!({
        "chain_id": chain_id,
        "rpc_url": format!("{}/rpc/{name}", server.uri()),
        "wss_url": null,
        "explorer_url": "https://explore.testnet.tempo.xyz",
        "default_stablecoin": TOKEN_ADDR,
        "task_bond_contract": TASK_BOND,
    })
}

/// A fresh in-memory node on `CHAIN_ID`, served for every RPC call to `url`.
fn node(url: &str) -> Arc<MockTempoRpc> {
    let mock = Arc::new(MockTempoRpc::new());
    mock.set_chain_id(CHAIN_ID);
    register_mock_rpc(url, mock.clone());
    mock
}

async fn requests_to(server: &MockServer, suffix: &str) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().ends_with(suffix))
        .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
        .collect()
}

// ─── escrow sign ──────────────────────────────────────────────────────────

fn escrow_params(chain_id: u64, deadline: i64) -> Value {
    json!({
        "bid_id": BID_ID,
        "task_id": "00000000-0000-0000-0000-0000000000cc",
        "amount": "75.00",
        "platform_fee_amount": "3.75",
        "worker_address": WORKER_ADDR,
        "arbitrator_address": ARBITRATOR_ADDR,
        "task_escrow_contract": TASK_ESCROW,
        "token_address": TOKEN_ADDR,
        "platform_wallet": PLATFORM_WALLET,
        "chain_id": chain_id as i64,
        "decimals": 6,
        "memo_text": null,
        "memo_hash": null,
        "poster_approval_deadline": deadline,
        "poster_approval_min_lifetime": MIN_LIFETIME,
    })
}

async fn mount_params(server: &MockServer, responses: Vec<(Value, u16)>) {
    for (i, (body, status)) in responses.into_iter().enumerate().rev() {
        Mock::given(method("GET"))
            .and(path(format!("/bids/{BID_ID}/escrow/params")))
            .and(query_param("review_clock", "2"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .up_to_n_times(1)
            .with_priority(i as u8 + 1)
            .mount(server)
            .await;
    }
}

fn sign_args(keys: &Keys) -> SignArgs {
    SignArgs {
        bid_id: BID_ID.into(),
        keystore: Some(keys.keystore.display().to_string()),
        wallet_password_file: Some(keys.password.clone()),
        wallet_address: None,
        rpc_url: None, // the deployment's proxy
        skip_allowance_check: false,
        receipt_timeout: None,
    }
}

async fn escrow_sign(server: &MockServer, args: SignArgs) -> Result<Envelope, CmdError> {
    taskfast_cli::cmd::escrow::run(&ctx_for(server), EscrowCommand::Sign(args)).await
}

async fn escrow_run(server: &MockServer, cmd: EscrowCommand) -> Result<Envelope, CmdError> {
    taskfast_cli::cmd::escrow::run(&ctx_for(server), cmd).await
}

fn selectors_sent(mock: &MockTempoRpc) -> Vec<String> {
    mock.sent_transactions()
        .iter()
        .map(|(_, calldata)| hex::encode(&calldata[..4]))
        .collect()
}

#[tokio::test]
async fn escrow_sign_prefunds_then_opens_and_finalizes_once() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    let (token, escrow) = (
        TOKEN_ADDR.parse::<Address>().unwrap(),
        TASK_ESCROW.parse::<Address>().unwrap(),
    );
    let total = U256::from(78_750_000u64); // deposit 75.00 + fee 3.75, 6 decimals
    mock.set_balance(token, keys.address, total);
    mount_params(&server, vec![(escrow_params(CHAIN_ID, FAR_FUTURE), 200)]).await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/escrow/finalize")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "bid_id": BID_ID, "task_id": "00000000-0000-0000-0000-0000000000cc",
            "status": "accepted", "task_status": "payment_pending",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let v = val(&escrow_sign(&server, sign_args(&keys))
        .await
        .expect("escrow opened"));
    assert_eq!(v["data"]["status"], "accepted");
    // Allowance started at 0: approve, then open. Nothing else was sent.
    assert_eq!(
        selectors_sent(&mock),
        vec![
            hex_sel(IERC20::approveCall::SELECTOR),
            hex_sel(TaskEscrow::openCall::SELECTOR),
        ]
    );
    // Both txs are signed for the verified chain (decoded from the signatures).
    assert_eq!(mock.broadcast_chain_ids(), [Some(CHAIN_ID), Some(CHAIN_ID)]);
    let (to, approve) = mock.sent_transactions()[0].clone();
    assert_eq!(to, token);
    let call = IERC20::approveCall::abi_decode(&approve).unwrap();
    assert_eq!(call.spender, escrow);
    assert_eq!(call.amount, total);
}

#[tokio::test]
async fn escrow_sign_refuses_an_underfunded_poster_before_any_tx() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    let token = TOKEN_ADDR.parse::<Address>().unwrap();
    // Funded for the deposit but not deposit + platform fee.
    mock.set_balance(token, keys.address, U256::from(75_000_000u64));
    mount_params(&server, vec![(escrow_params(CHAIN_ID, FAR_FUTURE), 200)]).await;

    let err = escrow_sign(&server, sign_args(&keys))
        .await
        .expect_err("underfunded");
    assert!(
        matches!(&err, CmdError::Usage(m) if m.contains("poster balance")),
        "{err:?}"
    );
    assert_eq!(mock.sent_transactions(), Vec::<(Address, Bytes)>::new());
    assert_eq!(
        requests_to(&server, "/escrow/finalize").await,
        Vec::<Value>::new()
    );
}

#[tokio::test]
async fn escrow_sign_balance_preflight_runs_even_when_the_allowance_check_is_skipped() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    mount_params(&server, vec![(escrow_params(CHAIN_ID, FAR_FUTURE), 200)]).await;
    let mut args = sign_args(&keys);
    args.skip_allowance_check = true;

    let err = escrow_sign(&server, args)
        .await
        .expect_err("skip-allowance must not skip the balance check");
    assert!(
        matches!(&err, CmdError::Usage(m) if m.contains("poster balance")),
        "{err:?}"
    );
    assert_eq!(mock.sent_transactions(), Vec::<(Address, Bytes)>::new());
}

#[tokio::test]
async fn escrow_sign_aborts_finalize_when_the_open_reverts_on_chain() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    let token = TOKEN_ADDR.parse::<Address>().unwrap();
    let escrow = TASK_ESCROW.parse::<Address>().unwrap();
    mock.set_balance(token, keys.address, U256::from(78_750_000u64));
    mock.set_allowance(token, keys.address, escrow, U256::MAX);
    mock.revert_receipt(TaskEscrow::openCall::SELECTOR);
    mount_params(&server, vec![(escrow_params(CHAIN_ID, FAR_FUTURE), 200)]).await;

    let err = escrow_sign(&server, sign_args(&keys))
        .await
        .expect_err("open reverted");
    match err {
        CmdError::Server(m) => {
            assert!(m.contains("open tx 0x"), "{m}");
            assert!(m.contains("reverted on-chain — finalize aborted"), "{m}");
        }
        other => panic!("expected Server, got {other:?}"),
    }
    assert_eq!(
        selectors_sent(&mock),
        vec![hex_sel(TaskEscrow::openCall::SELECTOR)]
    );
    assert_eq!(
        requests_to(&server, "/escrow/finalize").await,
        Vec::<Value>::new()
    );
}

#[tokio::test]
async fn escrow_sign_re_signs_the_same_voucher_when_the_deadline_goes_stale() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    let token = TOKEN_ADDR.parse::<Address>().unwrap();
    let escrow = TASK_ESCROW.parse::<Address>().unwrap();
    mock.set_balance(token, keys.address, U256::from(78_750_000u64));
    mock.set_allowance(token, keys.address, escrow, U256::MAX);
    let fresh_deadline = FAR_FUTURE + 86_400;
    mount_params(
        &server,
        vec![
            (escrow_params(CHAIN_ID, FAR_FUTURE), 200),
            (escrow_params(CHAIN_ID, fresh_deadline), 200),
        ],
    )
    .await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/escrow/finalize")))
        .and(wiremock::matchers::body_partial_json(json!({
            "poster_approval_deadline": FAR_FUTURE,
        })))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "deadline_below_minimum", "message": "stale",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/escrow/finalize")))
        .and(wiremock::matchers::body_partial_json(json!({
            "poster_approval_deadline": fresh_deadline,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "bid_id": BID_ID, "task_id": "00000000-0000-0000-0000-0000000000cc",
            "status": "accepted", "task_status": "payment_pending",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let v = val(&escrow_sign(&server, sign_args(&keys))
        .await
        .expect("re-signed"));
    assert_eq!(v["data"]["deadline"], fresh_deadline);
    // One on-chain open; the retry is a re-sign of the same voucher.
    assert_eq!(
        selectors_sent(&mock),
        vec![hex_sel(TaskEscrow::openCall::SELECTOR)]
    );
    let vouchers: Vec<Value> = requests_to(&server, "/escrow/finalize")
        .await
        .iter()
        .map(|b| b["voucher"].clone())
        .collect();
    assert_eq!(vouchers.len(), 2);
    assert_eq!(vouchers[0], vouchers[1]);
}

#[tokio::test]
async fn escrow_sign_refuses_a_node_on_another_chain_before_any_tx() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    mock.set_chain_id(CHAIN_ID + 1);
    mount_params(&server, vec![(escrow_params(CHAIN_ID, FAR_FUTURE), 200)]).await;

    let err = escrow_sign(&server, sign_args(&keys))
        .await
        .expect_err("node on the wrong chain");
    assert!(
        matches!(&err, CmdError::Validation { code, .. } if code == "chain_id_mismatch"),
        "{err:?}"
    );
    assert_eq!(mock.sent_transactions(), Vec::<(Address, Bytes)>::new());
}

#[tokio::test]
async fn escrow_sign_routes_a_zone_task_to_the_zones_own_proxy() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_network(&server, CHAIN_ID, Some(ZONE_CHAIN_ID)).await;
    let zone = node(&format!("{}/rpc/tempo-zone-moderato", server.uri()));
    zone.set_chain_id(ZONE_CHAIN_ID);
    let token = TOKEN_ADDR.parse::<Address>().unwrap();
    let escrow = TASK_ESCROW.parse::<Address>().unwrap();
    zone.set_balance(token, keys.address, U256::from(78_750_000u64));
    zone.set_allowance(token, keys.address, escrow, U256::MAX);
    mount_params(
        &server,
        vec![(escrow_params(ZONE_CHAIN_ID, FAR_FUTURE), 200)],
    )
    .await;
    Mock::given(method("POST"))
        .and(path(format!("/bids/{BID_ID}/escrow/finalize")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "bid_id": BID_ID, "task_id": "00000000-0000-0000-0000-0000000000cc",
            "status": "accepted", "task_status": "payment_pending",
        })))
        .mount(&server)
        .await;

    escrow_sign(&server, sign_args(&keys))
        .await
        .expect("zone escrow opened");
    assert_eq!(
        selectors_sent(&zone),
        vec![hex_sel(TaskEscrow::openCall::SELECTOR)]
    );
    assert_eq!(zone.broadcast_chain_ids(), [Some(ZONE_CHAIN_ID)]);
}

// ─── bond post ────────────────────────────────────────────────────────────

fn bond_args(keys: &Keys) -> PostArgs {
    PostArgs {
        task_id: BOND_TASK_ID.into(),
        task_bond: Some(TASK_BOND.into()),
        token: None,
        amount: None,
        source: BondStakeSource::OperatorSelf,
        keystore: Some(keys.keystore.display().to_string()),
        wallet_password_file: Some(keys.password.clone()),
        wallet_address: None,
        rpc_url: None,
        skip_allowance_check: false,
        receipt_timeout: None,
        verify_timeout: Some(std::time::Duration::ZERO),
    }
}

async fn mount_bond_quote(server: &MockServer, status: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{BOND_TASK_ID}/stake/quote")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "required_amount": 5_000_000,
            "bond_status": status,
            "tier": "high_assurance",
        })))
        .mount(server)
        .await;
}

async fn bond_post(server: &MockServer, keys: &Keys) -> Result<Envelope, CmdError> {
    taskfast_cli::cmd::bond::run(&ctx_for(server), BondCommand::Post(bond_args(keys))).await
}

#[tokio::test]
async fn bond_post_approves_posts_and_reports() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    let (token, bond) = (
        TOKEN_ADDR.parse::<Address>().unwrap(),
        TASK_BOND.parse::<Address>().unwrap(),
    );
    mock.set_balance(token, keys.address, U256::from(5_000_000u64));
    mount_bond_quote(&server, "none").await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{BOND_TASK_ID}/stake/report")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": BOND_TASK_ID, "status": "posted", "verification": "pending",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let v = val(&bond_post(&server, &keys).await.expect("bond posted"));
    let post_tx = v["data"]["post_tx_hash"].as_str().unwrap();
    assert_eq!(post_tx.len(), 66, "0x + 32 bytes: {post_tx}");
    assert_ne!(
        v["data"]["approval_tx_hash"],
        Value::Null,
        "allowance was 0 → approve sent"
    );
    assert_eq!(mock.broadcast_chain_ids(), [Some(CHAIN_ID), Some(CHAIN_ID)]);
    let sent = mock.sent_transactions();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].0, token, "approve targets the token");
    assert_eq!(sent[1].0, bond, "post targets the TaskBond contract");
    assert_eq!(&sent[1].1[..4], &TaskBond::postCall::SELECTOR);
}

#[tokio::test]
async fn bond_post_checks_the_balance_even_when_the_allowance_check_is_skipped() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    mount_bond_quote(&server, "none").await;
    let mut args = bond_args(&keys);
    args.skip_allowance_check = true;

    let err = taskfast_cli::cmd::bond::run(&ctx_for(&server), BondCommand::Post(args))
        .await
        .expect_err("no balance");
    assert!(
        matches!(&err, CmdError::Usage(m) if m.contains("poster balance")),
        "{err:?}"
    );
    assert_eq!(mock.sent_transactions(), Vec::<(Address, Bytes)>::new());
    assert_eq!(
        requests_to(&server, "/stake/report").await,
        Vec::<Value>::new()
    );
}

#[tokio::test]
async fn bond_post_refuses_a_node_on_another_chain() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    mock.set_chain_id(1);
    mount_bond_quote(&server, "none").await;

    let err = bond_post(&server, &keys).await.expect_err("node mismatch");
    assert!(
        matches!(&err, CmdError::Validation { code, .. } if code == "chain_id_mismatch"),
        "{err:?}"
    );
    assert_eq!(mock.sent_transactions(), Vec::<(Address, Bytes)>::new());
    assert_eq!(
        requests_to(&server, "/stake/report").await,
        Vec::<Value>::new()
    );
}

#[tokio::test]
async fn bond_post_never_reports_a_post_that_reverted() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    let token = TOKEN_ADDR.parse::<Address>().unwrap();
    mock.set_balance(token, keys.address, U256::from(5_000_000u64));
    let mut args = bond_args(&keys);
    args.skip_allowance_check = true;
    mount_bond_quote(&server, "none").await;
    mock.revert_estimate(TaskBond::postCall::SELECTOR);

    let err = taskfast_cli::cmd::bond::run(&ctx_for(&server), BondCommand::Post(args))
        .await
        .expect_err("estimate reverts");
    assert!(
        matches!(&err, CmdError::Server(m) if m.contains("execution reverted")),
        "{err:?}"
    );
    assert_eq!(mock.sent_transactions(), Vec::<(Address, Bytes)>::new());
    assert_eq!(
        requests_to(&server, "/stake/report").await,
        Vec::<Value>::new()
    );
}

// ─── escalate ─────────────────────────────────────────────────────────────

fn escalate_sign_args(keys: &Keys) -> EscalateSignArgs {
    EscalateSignArgs {
        task_id: TASK_ID.into(),
        keystore: Some(keys.keystore.display().to_string()),
        wallet_password_file: Some(keys.password.clone()),
        wallet_address: None,
        rpc_url: None,
        receipt_timeout: None,
    }
}

fn tr_args(keys: &Keys) -> TimeoutResolveArgs {
    TimeoutResolveArgs {
        task_id: TASK_ID.into(),
        keystore: Some(keys.keystore.display().to_string()),
        wallet_password_file: Some(keys.password.clone()),
        wallet_address: None,
        rpc_url: None,
        receipt_timeout: None,
    }
}

async fn mount_signing_params(server: &MockServer, chain_id: u64, poster: Address) {
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}/dispute/signing-params")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": TASK_ID,
            "phase": "awaiting_signature",
            "escalation_deadline": "2026-12-01T00:00:00Z",
            "lapse_pending": false,
            "chain_id": chain_id as i64,
            "task_escrow_contract": TASK_ESCROW,
            "escrow_id": ESCROW_ID,
            "poster_wallet_address": format!("{poster:#x}"),
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn escalate_sign_broadcasts_the_dispute_from_the_poster_wallet() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    mount_signing_params(&server, CHAIN_ID, keys.address).await;

    let env = taskfast_cli::cmd::escalate::run(
        &ctx_for(&server),
        EscalateCommand::Sign(escalate_sign_args(&keys)),
    )
    .await
    .expect("dispute submitted");
    let v = val(&env);
    assert_eq!(v["data"]["action"], "dispute_submitted");
    assert_eq!(v["data"]["tx_hash"].as_str().unwrap().len(), 66);
    let sent = mock.sent_transactions();
    assert_eq!(sent.len(), 1);
    assert_eq!(mock.broadcast_chain_ids(), [Some(CHAIN_ID)]);
    assert_eq!(sent[0].0, TASK_ESCROW.parse::<Address>().unwrap());
    assert_eq!(
        &sent[0].1[..4],
        &taskfast_agent::chain::dispute_calldata(ESCROW_ID.parse().unwrap())[..4]
    );
}

#[tokio::test]
async fn escalate_sign_accepts_a_zone_chain_the_deployment_advertises() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_network(&server, CHAIN_ID, Some(ZONE_CHAIN_ID)).await;
    let zone = node(&format!("{}/rpc/tempo-zone-moderato", server.uri()));
    zone.set_chain_id(ZONE_CHAIN_ID);
    mount_signing_params(&server, ZONE_CHAIN_ID, keys.address).await;

    taskfast_cli::cmd::escalate::run(
        &ctx_for(&server),
        EscalateCommand::Sign(escalate_sign_args(&keys)),
    )
    .await
    .expect("zone dispute submitted");
    assert_eq!(zone.sent_transactions().len(), 1);
    assert_eq!(zone.broadcast_chain_ids(), [Some(ZONE_CHAIN_ID)]);
}

#[tokio::test]
async fn escalate_sign_refuses_a_chain_the_deployment_does_not_advertise() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_network(&server, CHAIN_ID, None).await;
    let mock = node(&format!("{}/rpc/testnet", server.uri()));
    mount_signing_params(&server, 1, keys.address).await;

    let err = taskfast_cli::cmd::escalate::run(
        &ctx_for(&server),
        EscalateCommand::Sign(escalate_sign_args(&keys)),
    )
    .await
    .expect_err("unadvertised chain");
    assert!(
        matches!(&err, CmdError::Validation { code, .. } if code == "chain_id_mismatch"),
        "{err:?}"
    );
    assert_eq!(mock.sent_transactions(), Vec::<(Address, Bytes)>::new());
}

/// The by-chain-id lookup never crosses networks: a testnet environment
/// refuses a server-issued mainnet chain id even when the deployment
/// advertises a mainnet entry (the rule `connect_rpc` got in 60de0d0, now in
/// the seam).
#[tokio::test]
async fn escalate_sign_never_follows_a_chain_id_across_networks() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_networks(
        &server,
        json!({
            "testnet": entry(&server, "testnet", CHAIN_ID),
            "mainnet": entry(&server, "mainnet", MAINNET_CHAIN_ID),
        }),
    )
    .await;
    let mock = node(&format!("{}/rpc/mainnet", server.uri()));
    mock.set_chain_id(MAINNET_CHAIN_ID);
    mount_signing_params(&server, MAINNET_CHAIN_ID, keys.address).await;

    let err = taskfast_cli::cmd::escalate::run(
        &ctx_for(&server),
        EscalateCommand::Sign(escalate_sign_args(&keys)),
    )
    .await
    .expect_err("a testnet run must not follow a mainnet chain id");
    assert!(
        matches!(&err, CmdError::Validation { code, .. } if code == "chain_id_mismatch"),
        "{err:?}"
    );
    assert_eq!(mock.request_count(), 0, "no node I/O before the refusal");
}

/// A prod run follows a server-issued mainnet Tempo Zone chain id: a zone's
/// chain id is on neither L1, so only "not the opposite L1" can be asked of
/// it.
#[tokio::test]
async fn escalate_sign_on_prod_follows_a_mainnet_zone_chain_id() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_networks(
        &server,
        json!({
            "mainnet": entry(&server, "mainnet", MAINNET_CHAIN_ID),
            "tempo-zone-prod": entry(&server, "tempo-zone-prod", MAINNET_ZONE_CHAIN_ID),
        }),
    )
    .await;
    let zone = node(&format!("{}/rpc/tempo-zone-prod", server.uri()));
    zone.set_chain_id(MAINNET_ZONE_CHAIN_ID);
    mount_signing_params(&server, MAINNET_ZONE_CHAIN_ID, keys.address).await;

    taskfast_cli::cmd::escalate::run(
        &prod_ctx_for(&server),
        EscalateCommand::Sign(escalate_sign_args(&keys)),
    )
    .await
    .expect("prod zone dispute submitted");
    assert_eq!(zone.broadcast_chain_ids(), [Some(MAINNET_ZONE_CHAIN_ID)]);
}

/// A prod run still refuses the testnet L1's chain id, advertised or not.
#[tokio::test]
async fn escalate_sign_on_prod_refuses_the_testnet_chain_id() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_networks(
        &server,
        json!({
            "mainnet": entry(&server, "mainnet", MAINNET_CHAIN_ID),
            "testnet": entry(&server, "testnet", CHAIN_ID),
        }),
    )
    .await;
    let testnet = node(&format!("{}/rpc/testnet", server.uri()));
    mount_signing_params(&server, CHAIN_ID, keys.address).await;

    let err = taskfast_cli::cmd::escalate::run(
        &prod_ctx_for(&server),
        EscalateCommand::Sign(escalate_sign_args(&keys)),
    )
    .await
    .expect_err("testnet chain on prod");
    assert!(
        matches!(&err, CmdError::Validation { code, .. } if code == "chain_id_mismatch"),
        "{err:?}"
    );
    assert_eq!(testnet.request_count(), 0, "no node I/O before the refusal");
}

/// A mainnet zone's chain id is not in the policy table; a prod run still
/// enforces mainnet's HTTPS rule on an explicit `--rpc-url`.
#[tokio::test]
async fn escalate_sign_on_prod_refuses_a_plain_http_override_for_a_zone() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_networks(
        &server,
        json!({
            "mainnet": entry(&server, "mainnet", MAINNET_CHAIN_ID),
            "tempo-zone-prod": entry(&server, "tempo-zone-prod", MAINNET_ZONE_CHAIN_ID),
        }),
    )
    .await;
    mount_signing_params(&server, MAINNET_ZONE_CHAIN_ID, keys.address).await;
    let mut args = escalate_sign_args(&keys);
    args.rpc_url = Some("http://rpc.invalid".into());

    let err = taskfast_cli::cmd::escalate::run(&prod_ctx_for(&server), EscalateCommand::Sign(args))
        .await
        .expect_err("plain HTTP on prod");
    assert!(
        matches!(&err, CmdError::Usage(m) if m.contains("plain-HTTP mainnet")),
        "{err:?}"
    );
}

#[tokio::test]
async fn escalate_sign_reports_a_reverted_dispute() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    mount_signing_params(&server, CHAIN_ID, keys.address).await;
    let calldata = taskfast_agent::chain::dispute_calldata(ESCROW_ID.parse().unwrap());
    mock.revert_receipt(sel(&calldata));

    let err = taskfast_cli::cmd::escalate::run(
        &ctx_for(&server),
        EscalateCommand::Sign(escalate_sign_args(&keys)),
    )
    .await
    .expect_err("reverted");
    assert!(
        matches!(&err, CmdError::Server(m) if m.contains("reverted on-chain (Unauthorized")),
        "{err:?}"
    );
}

async fn mount_disputed_task(server: &MockServer, chain_id: u64) {
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": TASK_ID,
            "status": "disputed",
            "escrow_id": ESCROW_ID,
            "settlement_domain": { "chain_id": chain_id as i64, "verifying_contract": TASK_ESCROW },
        })))
        .mount(server)
        .await;
}

/// Serve the reads `timeout-resolve` makes from fixed chain facts.
fn disputed_chain(mock: &MockTempoRpc, state: u8, block_ts: u64) {
    let id: B256 = ESCROW_ID.parse().unwrap();
    // getEscrow: 8 words, `state` last.
    let state_word = format!("{:064x}", u64::from(state));
    let get_escrow = format!("{}{state_word}", "00".repeat(32 * 7));
    mock.set_call_result(
        sel(&get_escrow_calldata(id)),
        hex::decode(get_escrow).unwrap(),
    );
    mock.set_call_result(
        sel(&disputed_at_calldata(id)),
        hex::decode(word(DISPUTED_AT)).unwrap(),
    );
    mock.set_call_result(
        sel(&dispute_timeout_calldata()),
        hex::decode(word(TIMEOUT)).unwrap(),
    );
    mock.set_raw(
        "eth_getBlockByNumber",
        json!({ "timestamp": format!("0x{block_ts:x}") }),
    );
}

#[tokio::test]
async fn timeout_resolve_sends_once_the_chain_clock_passes_the_deadline() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    disputed_chain(&mock, 5, DISPUTED_AT + TIMEOUT);
    mount_disputed_task(&server, CHAIN_ID).await;

    let env = taskfast_cli::cmd::escalate::run(
        &ctx_for(&server),
        EscalateCommand::TimeoutResolve(tr_args(&keys)),
    )
    .await
    .expect("timeout-resolve sent");
    let v = val(&env);
    assert_eq!(v["data"]["action"], "timeout_resolve_submitted");
    assert_eq!(v["data"]["tx_hash"].as_str().unwrap().len(), 66);
    let sent = mock.sent_transactions();
    assert_eq!(sent.len(), 1);
    assert_eq!(mock.broadcast_chain_ids(), [Some(CHAIN_ID)]);
    assert_eq!(
        &sent[0].1[..4],
        &timeout_resolve_calldata(ESCROW_ID.parse().unwrap())[..4]
    );
}

#[tokio::test]
async fn timeout_resolve_sends_nothing_for_a_non_disputed_escrow() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    disputed_chain(&mock, 1, DISPUTED_AT + TIMEOUT);
    mount_disputed_task(&server, CHAIN_ID).await;

    let err = taskfast_cli::cmd::escalate::run(
        &ctx_for(&server),
        EscalateCommand::TimeoutResolve(tr_args(&keys)),
    )
    .await
    .expect_err("not disputed");
    assert!(
        matches!(&err, CmdError::Validation { code, .. } if code == "escrow_not_disputed"),
        "{err:?}"
    );
    assert_eq!(mock.sent_transactions(), Vec::<(Address, Bytes)>::new());
}

#[tokio::test]
async fn timeout_resolve_judges_elapsed_time_by_the_chain_not_the_local_clock() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mock = chain(&server).await;
    // The local clock is long past the deadline; the latest block is not.
    disputed_chain(&mock, 5, DISPUTED_AT + TIMEOUT - 1);
    mount_disputed_task(&server, CHAIN_ID).await;

    let err = taskfast_cli::cmd::escalate::run(
        &ctx_for(&server),
        EscalateCommand::TimeoutResolve(tr_args(&keys)),
    )
    .await
    .expect_err("too early");
    assert!(
        matches!(&err, CmdError::Validation { code, .. } if code == "dispute_timeout_not_elapsed"),
        "{err:?}"
    );
    assert_eq!(mock.sent_transactions(), Vec::<(Address, Bytes)>::new());
}

// ─── escrow submit-work ───────────────────────────────────────────────────

#[tokio::test]
async fn submit_work_starts_a_zone_escrows_review_clock_on_the_zone_chain() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_network(&server, CHAIN_ID, Some(ZONE_CHAIN_ID)).await;
    let zone = node(&format!("{}/rpc/tempo-zone-moderato", server.uri()));
    zone.set_chain_id(ZONE_CHAIN_ID);
    let id: B256 = ESCROW_ID.parse().unwrap();
    zone.set_call_result(
        sel(&[0xec, 0x85, 0x3c, 0x7d]), // reviewWindowOf
        hex::decode(word(86_400)).unwrap(),
    );
    // getEscrow: poster, worker, platform, arbitrator, token, deposit, fee, state.
    let worker_word = format!("{:064x}", U256::from_be_slice(keys.address.as_slice()));
    let state_word = format!("{:064x}", 1u64);
    let get_escrow = format!(
        "{word0}{worker_word}{}{state_word}",
        "00".repeat(32 * 5),
        word0 = word(0)
    );
    zone.set_call_result(
        sel(&get_escrow_calldata(id)),
        hex::decode(get_escrow).unwrap(),
    );
    zone.set_call_result(
        sel(&[0x53, 0xff, 0xf8, 0xac]),
        hex::decode(word(0)).unwrap(),
    ); // submittedAtOf
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": TASK_ID,
            "status": "under_review",
            "review_deadline": "2027-01-15T08:00:00Z",
            "escrow_id": ESCROW_ID,
            "settlement_domain": { "chain_id": ZONE_CHAIN_ID as i64, "verifying_contract": TASK_ESCROW },
        })))
        .mount(&server)
        .await;

    let args = SubmitWorkArgs {
        task_id: TASK_ID.into(),
        keystore: Some(keys.keystore.display().to_string()),
        wallet_password_file: Some(keys.password.clone()),
        wallet_address: None,
        rpc_url: None,
        receipt_timeout: None,
    };
    let v = val(&escrow_run(&server, EscrowCommand::SubmitWork(args))
        .await
        .expect("clock started"));
    assert_eq!(v["data"]["action"], "submit_work_submitted");
    let sent = zone.sent_transactions();
    assert_eq!(sent.len(), 1);
    assert_eq!(&sent[0].1[..4], &[0x3a, 0x9c, 0xfe, 0xf2]); // submitWork(bytes32)
    assert_eq!(zone.broadcast_chain_ids(), [Some(ZONE_CHAIN_ID)]);
}
