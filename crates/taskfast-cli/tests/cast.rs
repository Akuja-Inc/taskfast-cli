// SPDX-License-Identifier: MIT
//! End-to-end tests for `taskfast cast` (gh#101).
//!
//! Unlike `bond post` / `escrow sign`, `cast` needs no TaskFast API mock when
//! `--rpc-url` is given: the override path talks straight to the RPC endpoint,
//! so a single wiremock server standing in for the Tempo node covers all three
//! verbs — including the full live `send` leg (chainId → nonce → gasPrice →
//! estimateGas → sendRawTransaction → receipt), which bond/escrow defer to
//! manual E2E because their RPC URL arrives via the authenticated config path.

use std::path::PathBuf;

use alloy_signer_local::PrivateKeySigner;
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::cast::{run, CallArgs, Command, RpcArgs, SendArgs};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const TARGET: &str = "0x31de2fd7d1d4bfcfb3d2b4bfc30f6b46f2b55db2";
const HOLDER: &str = "0x00000000000000000000000000000000000000aa";

fn ctx(dry_run: bool) -> Ctx {
    Ctx {
        environment: Environment::Local,
        config_path: PathBuf::from("/dev/null"),
        dry_run,
        quiet: true,
        allow_custom_endpoints: true,
        ..Default::default()
    }
}

fn envelope_value(env: &Envelope) -> Value {
    serde_json::to_value(env).expect("serialize envelope")
}

/// JSON-RPC success envelope, mirroring `tempo_rpc.rs`'s test helper.
fn rpc_ok(result: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": result,
    }))
}

async fn mount_rpc(server: &MockServer, rpc_method: &str, result: Value) {
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_partial_json(json!({"method": rpc_method})))
        .respond_with(rpc_ok(result))
        .mount(server)
        .await;
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

fn send_args(keys: &Keys, rpc_url: String) -> SendArgs {
    SendArgs {
        to: TARGET.into(),
        sig: "approve(address,uint256)".into(),
        args: vec![HOLDER.into(), "5000000".into()],
        keystore: Some(keys.keystore_path.display().to_string()),
        wallet_password_file: Some(keys.password_path.clone()),
        wallet_address: None,
        rpc_url: Some(rpc_url),
        receipt_timeout: None,
    }
}

// ---------------------------------------------------------------- call

#[tokio::test]
async fn call_decodes_declared_outputs() {
    let server = MockServer::start().await;
    // bool true, ABI-encoded: 32 bytes ending in 0x01.
    let word = format!("0x{}{}", "00".repeat(31), "01");
    mount_rpc(&server, "eth_call", json!(word)).await;

    let env = run(
        &ctx(false),
        Command::Call(CallArgs {
            to: TARGET.into(),
            sig: "paused()(bool)".into(),
            args: vec![],
            rpc_url: Some(server.uri()),
        }),
    )
    .await
    .expect("call succeeds");

    let v = envelope_value(&env);
    assert_eq!(v["data"]["decoded"], json!([true]));
    assert_eq!(v["data"]["raw"], json!(word));
}

#[tokio::test]
async fn call_without_declared_outputs_returns_raw_only() {
    let server = MockServer::start().await;
    let word = format!("0x{}{}", "00".repeat(31), "01");
    mount_rpc(&server, "eth_call", json!(word)).await;

    let env = run(
        &ctx(false),
        Command::Call(CallArgs {
            to: TARGET.into(),
            sig: "paused()".into(),
            args: vec![],
            rpc_url: Some(server.uri()),
        }),
    )
    .await
    .expect("call succeeds");

    let v = envelope_value(&env);
    assert_eq!(v["data"]["decoded"], Value::Null);
    assert_eq!(v["data"]["raw"], json!(word));
}

#[tokio::test]
async fn call_rejects_invalid_address() {
    let err = run(
        &ctx(false),
        Command::Call(CallArgs {
            to: "not-an-address".into(),
            sig: "paused()(bool)".into(),
            args: vec![],
            rpc_url: Some("http://rpc.invalid".into()),
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn call_rejects_arity_mismatch_before_network() {
    // No mock server at all — the arity check must fire before any I/O.
    let err = run(
        &ctx(false),
        Command::Call(CallArgs {
            to: TARGET.into(),
            sig: "balanceOf(address)(uint256)".into(),
            args: vec![],
            rpc_url: Some("http://rpc.invalid".into()),
        }),
    )
    .await
    .unwrap_err();
    match err {
        CmdError::Usage(m) => assert!(m.contains("takes 1"), "msg: {m}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn custom_rpc_url_requires_allow_flag() {
    let mut c = ctx(false);
    c.allow_custom_endpoints = false;
    let err = run(
        &c,
        Command::Call(CallArgs {
            to: TARGET.into(),
            sig: "paused()(bool)".into(),
            args: vec![],
            rpc_url: Some("http://192.0.2.1:8545".into()),
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

// ---------------------------------------------------------------- send

#[tokio::test]
async fn send_dry_run_builds_calldata_without_network() {
    let keys = fresh_keys();
    // rpc.invalid is never resolved: dry-run short-circuits before I/O.
    let env = run(
        &ctx(true),
        Command::Send(send_args(&keys, "http://rpc.invalid".into())),
    )
    .await
    .expect("dry-run succeeds");

    let v = envelope_value(&env);
    assert_eq!(v["data"]["action"], json!("would_send"));
    let calldata = v["data"]["calldata"].as_str().expect("calldata string");
    // IERC20.approve selector.
    assert!(calldata.starts_with("0x095ea7b3"), "calldata: {calldata}");
    // selector + 2 words, hex-encoded with 0x prefix.
    assert_eq!(calldata.len(), 2 + 2 * (4 + 64));
}

#[tokio::test]
async fn send_broadcasts_and_reports_tx_hash() {
    let server = MockServer::start().await;
    let tx_hash = format!("0x{}", "ab".repeat(32));
    mount_rpc(&server, "eth_chainId", json!("0xa5bf")).await;
    mount_rpc(&server, "eth_getTransactionCount", json!("0x0")).await;
    mount_rpc(&server, "eth_gasPrice", json!("0x3b9aca00")).await;
    mount_rpc(&server, "eth_estimateGas", json!("0x5208")).await;
    mount_rpc(&server, "eth_sendRawTransaction", json!(tx_hash)).await;
    mount_rpc(
        &server,
        "eth_getTransactionReceipt",
        json!({"status": "0x1"}),
    )
    .await;

    let keys = fresh_keys();
    let env = run(&ctx(false), Command::Send(send_args(&keys, server.uri())))
        .await
        .expect("send succeeds");

    let v = envelope_value(&env);
    assert_eq!(v["data"]["action"], json!("sent"));
    assert_eq!(v["data"]["tx_hash"], json!(tx_hash));
}

#[tokio::test]
async fn send_rejects_wallet_address_mismatch() {
    let keys = fresh_keys();
    let mut args = send_args(&keys, "http://rpc.invalid".into());
    args.wallet_address = Some(HOLDER.into()); // random keystore ≠ fixed addr
    let err = run(&ctx(true), Command::Send(args)).await.unwrap_err();
    match err {
        CmdError::Usage(m) => assert!(m.contains("does not match"), "msg: {m}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

// ---------------------------------------------------------------- rpc

#[tokio::test]
async fn rpc_passes_through_method_params_and_result() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_partial_json(json!({
            "method": "tempo_fundAddress",
            "params": [HOLDER],
        })))
        .respond_with(rpc_ok(json!({"funded": true})))
        .mount(&server)
        .await;

    let env = run(
        &ctx(false),
        Command::Rpc(RpcArgs {
            method: "tempo_fundAddress".into(),
            params: Some(format!("[\"{HOLDER}\"]")),
            rpc_url: Some(server.uri()),
        }),
    )
    .await
    .expect("rpc succeeds");

    let v = envelope_value(&env);
    assert_eq!(v["data"]["result"], json!({"funded": true}));
}

#[tokio::test]
async fn rpc_dry_run_short_circuits() {
    // A raw passthrough can't distinguish reads from mutations, so dry-run
    // must not touch the network at all (rpc.invalid would fail if it did).
    let env = run(
        &ctx(true),
        Command::Rpc(RpcArgs {
            method: "eth_sendRawTransaction".into(),
            params: Some("[\"0xdead\"]".into()),
            rpc_url: Some("http://rpc.invalid".into()),
        }),
    )
    .await
    .expect("dry-run succeeds");

    let v = envelope_value(&env);
    assert_eq!(v["data"]["action"], json!("would_rpc"));
    assert_eq!(v["data"]["params"], json!(["0xdead"]));
}

#[tokio::test]
async fn rpc_rejects_malformed_params_json() {
    let err = run(
        &ctx(false),
        Command::Rpc(RpcArgs {
            method: "eth_chainId".into(),
            params: Some("not json".into()),
            rpc_url: Some("http://rpc.invalid".into()),
        }),
    )
    .await
    .unwrap_err();
    match err {
        CmdError::Usage(m) => assert!(m.contains("not valid JSON"), "msg: {m}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}
