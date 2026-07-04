// SPDX-License-Identifier: MIT
//! End-to-end tests for `taskfast bond post` (gh#95).
//!
//! Dry-run covers the happy path without RPC mocks (no tx broadcast): it still
//! exercises the quote fetch, network-config resolution (token from
//! `default_stablecoin`), `taskRef` derivation, and calldata build. Error-
//! mapping tests pin the Auth-vs-Validation contract (exit 2 vs 4) for the
//! quote endpoint, plus the local-only guards (bad UUID, amount-below-quote).
//!
//! The live on-chain path (approve + `TaskBond.post` + report + verify poll)
//! requires mocking the full Tempo RPC surface — deferred to manual E2E, as in
//! `tests/escrow.rs`.

use std::path::PathBuf;

use alloy_signer_local::PrivateKeySigner;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::bond::{run, BondStakeSource, Command, PostArgs};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const TASK_ID: &str = "00112233-4455-6677-8899-aabbccddeeff";
const TASK_BOND: &str = "0x31de2fd7d1d4bfcfb3d2b4bfc30f6b46f2b55db2";
const STABLECOIN: &str = "0x20C0000000000000000000000000000000000000";
const REQUIRED_AMOUNT: i64 = 5_000_000;

fn ctx_for(server: &MockServer, key: Option<&str>) -> Ctx {
    Ctx {
        api_key: key.map(String::from),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run: false,
        quiet: true,
        allow_custom_endpoints: true,
        ..Default::default()
    }
}

fn envelope_value(env: &Envelope) -> Value {
    serde_json::to_value(env).expect("serialize envelope")
}

struct Keys {
    _tmp: tempfile::TempDir,
    keystore_path: PathBuf,
    password_path: PathBuf,
}

fn fresh_keys() -> Keys {
    let signer = PrivateKeySigner::random();
    let tmp = tempfile::tempdir().expect("tempdir");
    let keystore_path = tmp.path().join("wallet.json");
    taskfast_agent::keystore::save_signer(&signer, &keystore_path, "pw").expect("keystore");
    let password_path = tmp.path().join("pw");
    std::fs::write(&password_path, b"pw").expect("write password");
    Keys {
        _tmp: tmp,
        keystore_path,
        password_path,
    }
}

fn base_args(keys: &Keys) -> PostArgs {
    PostArgs {
        task_id: TASK_ID.into(),
        task_bond: TASK_BOND.into(),
        token: None, // exercises the default_stablecoin path
        amount: None,
        source: BondStakeSource::OperatorSelf,
        keystore: Some(keys.keystore_path.display().to_string()),
        wallet_password_file: Some(keys.password_path.clone()),
        wallet_address: None,
        rpc_url: Some("http://rpc.invalid".into()), // never hit in dry-run
        skip_allowance_check: false,
        receipt_timeout: None,
        verify_timeout: None,
    }
}

fn quote_json(bond_status: Option<&str>) -> Value {
    json!({
        "required_amount": REQUIRED_AMOUNT,
        "bond_status": bond_status,
        "tier": "high_assurance",
    })
}

fn network_config_json() -> Value {
    json!({
        "networks": {
            "testnet": {
                "chain_id": 42_431,
                "rpc_url": "http://127.0.0.1:4000/rpc/testnet",
                "wss_url": "wss://rpc.moderato.tempo.xyz",
                "explorer_url": "https://explore.testnet.tempo.xyz",
                "default_stablecoin": STABLECOIN,
            }
        }
    })
}

async fn mount_quote(server: &MockServer, status: u16, body: Value) {
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}/stake/quote")))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn mount_network_config(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/config/network"))
        .respond_with(ResponseTemplate::new(200).set_body_json(network_config_json()))
        .mount(server)
        .await;
}

#[tokio::test]
async fn dry_run_emits_would_post_bond_with_derived_task_ref_and_default_token() {
    let server = MockServer::start().await;
    mount_quote(&server, 200, quote_json(None)).await;
    mount_network_config(&server).await;
    let keys = fresh_keys();

    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;

    let env = run(&ctx, Command::Post(base_args(&keys)))
        .await
        .expect("dry-run ok");
    let v = envelope_value(&env);

    assert_eq!(v["dry_run"], true);
    let data = &v["data"];
    assert_eq!(data["action"], "would_post_bond");
    assert_eq!(data["task_id"], TASK_ID);
    // token defaults to the deployment's default_stablecoin (lower-cased hex).
    assert_eq!(
        data["token"].as_str().unwrap().to_lowercase(),
        STABLECOIN.to_lowercase()
    );
    assert_eq!(data["amount"], REQUIRED_AMOUNT);
    // taskRef = 16 zero bytes ++ task UUID (32 bytes → 66-char 0x hex).
    let task_ref = data["task_ref"].as_str().unwrap();
    assert_eq!(task_ref.len(), 66, "task_ref must be 0x + 64 hex chars");
    assert!(
        task_ref.starts_with("0x00000000000000000000000000000000"),
        "high 16 bytes must be zero: {task_ref}"
    );
    assert!(task_ref.ends_with("00112233445566778899aabbccddeeff"));
    // salt is random 32 bytes; calldata is present.
    assert_eq!(data["salt"].as_str().unwrap().len(), 66);
    assert!(data["post_calldata"].as_str().unwrap().starts_with("0x"));
}

#[tokio::test]
async fn already_posted_bond_short_circuits() {
    let server = MockServer::start().await;
    mount_quote(&server, 200, quote_json(Some("posted"))).await;
    let keys = fresh_keys();
    let ctx = ctx_for(&server, Some("test-key"));

    let env = run(&ctx, Command::Post(base_args(&keys)))
        .await
        .expect("already-posted is success");
    let v = envelope_value(&env);
    assert_eq!(v["data"]["action"], "already_posted");
    assert_eq!(v["data"]["bond_status"], "posted");
}

#[tokio::test]
async fn amount_below_quote_is_usage_error() {
    let server = MockServer::start().await;
    mount_quote(&server, 200, quote_json(None)).await;
    let keys = fresh_keys();
    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;

    let mut args = base_args(&keys);
    args.amount = Some(REQUIRED_AMOUNT - 1);
    let err = run(&ctx, Command::Post(args))
        .await
        .expect_err("below-quote must fail");
    match err {
        CmdError::Usage(msg) => assert!(msg.contains("required_amount"), "msg: {msg}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn bad_task_uuid_is_usage_error_without_http() {
    let server = MockServer::start().await; // no mocks: must fail before any request
    let keys = fresh_keys();
    let ctx = ctx_for(&server, Some("test-key"));

    let mut args = base_args(&keys);
    args.task_id = "not-a-uuid".into();
    let err = run(&ctx, Command::Post(args))
        .await
        .expect_err("bad uuid must fail");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn bad_task_bond_address_is_usage_error() {
    let server = MockServer::start().await;
    mount_quote(&server, 200, quote_json(None)).await;
    let keys = fresh_keys();
    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;

    let mut args = base_args(&keys);
    args.task_bond = "0xnothex".into();
    let err = run(&ctx, Command::Post(args))
        .await
        .expect_err("bad task-bond must fail");
    match err {
        CmdError::Usage(msg) => assert!(msg.contains("task-bond"), "msg: {msg}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn quote_403_maps_to_auth() {
    let server = MockServer::start().await;
    mount_quote(&server, 403, json!({ "error": "not_operator_of_record" })).await;
    let keys = fresh_keys();
    let ctx = ctx_for(&server, Some("test-key"));

    let err = run(&ctx, Command::Post(base_args(&keys)))
        .await
        .expect_err("403 must fail");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn quote_409_maps_to_validation() {
    let server = MockServer::start().await;
    mount_quote(&server, 409, json!({ "error": "not_high_assurance" })).await;
    let keys = fresh_keys();
    let ctx = ctx_for(&server, Some("test-key"));

    let err = run(&ctx, Command::Post(base_args(&keys)))
        .await
        .expect_err("409 must fail");
    assert!(matches!(err, CmdError::Validation { .. }), "got {err:?}");
}
