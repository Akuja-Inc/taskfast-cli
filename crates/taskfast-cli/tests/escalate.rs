// SPDX-License-Identifier: MIT
//! End-to-end tests for `taskfast escalate` (poster's on-chain dispute).
//!
//! wiremock only: no chain, no live RPC. The live send needs a full Tempo RPC
//! mock (chainId, nonce, gas, sendRawTransaction, receipt) and stays deferred,
//! as in tests/escrow.rs. `--dry-run` covers calldata; the node-chain-id guard
//! is covered by a one-method RPC mock that must see no `eth_sendRawTransaction`.

use std::path::PathBuf;

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::escalate::{run, Command, RejectRemedyArgs, SignArgs, TimeoutResolveArgs};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const TASK_ID: &str = "00000000-0000-0000-0000-0000000000aa";
const CONTRACT: &str = "0x0000000000000000000000000000000000000001";
const ESCROW_ID: &str = "0xabababababababababababababababababababababababababababababababab";
const CHAIN_ID: i64 = 42_431;
const DEADLINE: &str = "2026-12-01T00:00:00Z";

fn ctx_for(server: &MockServer) -> Ctx {
    Ctx {
        api_key: Some("k".into()),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path: PathBuf::from("/dev/null"),
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
    address: Address,
}

fn fresh_keys() -> Keys {
    let signer = PrivateKeySigner::random();
    let tmp = tempfile::tempdir().expect("tempdir");
    let keystore_path = tmp.path().join("wallet.json");
    taskfast_agent::keystore::save_signer(&signer, &keystore_path, "pw").expect("keystore");
    let password_path = tmp.path().join("pw");
    std::fs::write(&password_path, b"pw").expect("write password");
    Keys {
        address: signer.address(),
        _tmp: tmp,
        keystore_path,
        password_path,
    }
}

fn sign_args(keys: &Keys, rpc_url: &str) -> SignArgs {
    SignArgs {
        task_id: TASK_ID.into(),
        keystore: Some(keys.keystore_path.display().to_string()),
        wallet_password_file: Some(keys.password_path.clone()),
        wallet_address: None,
        rpc_url: Some(rpc_url.into()),
        receipt_timeout: None,
    }
}

fn params_json(
    poster: Option<String>,
    chain_id: i64,
    lapse_pending: bool,
    contract: &str,
) -> Value {
    json!({
        "task_id": TASK_ID,
        "phase": "awaiting_signature",
        "escalation_deadline": DEADLINE,
        "lapse_pending": lapse_pending,
        "chain_id": chain_id,
        "task_escrow_contract": contract,
        "escrow_id": ESCROW_ID,
        "poster_wallet_address": poster,
    })
}

async fn mount_params(server: &MockServer, body: Value) {
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}/dispute/signing-params")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn mount_network(server: &MockServer, chain_id: i64) {
    Mock::given(method("GET"))
        .and(path("/config/network"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "networks": { "testnet": {
                "chain_id": chain_id,
                "rpc_url": format!("{}/rpc/testnet", server.uri()),
                "wss_url": null,
                "explorer_url": "https://explore.testnet.tempo.xyz",
            }}
        })))
        .mount(server)
        .await;
}

async fn dry_sign(server: &MockServer, keys: &Keys) -> Result<Envelope, CmdError> {
    let mut ctx = ctx_for(server);
    ctx.dry_run = true;
    run(&ctx, Command::Sign(sign_args(keys, "http://rpc.invalid"))).await
}

async fn requests_to(server: &MockServer, prefix: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with(prefix))
        .count()
}

// ─── reject-remedy ────────────────────────────────────────────────────────

fn reject() -> Command {
    Command::RejectRemedy(RejectRemedyArgs {
        task_id: TASK_ID.into(),
    })
}

#[tokio::test]
async fn reject_remedy_200_returns_phase_and_deadline() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/dispute/reject-remedy")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": TASK_ID,
            "phase": "awaiting_signature",
            "escalation_deadline": DEADLINE,
        })))
        .expect(1)
        .mount(&server)
        .await;
    let v = envelope_value(&run(&ctx_for(&server), reject()).await.expect("ok"));
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["phase"], "awaiting_signature");
    assert_eq!(v["data"]["escalation_deadline"], DEADLINE);
}

#[tokio::test]
async fn reject_remedy_bad_uuid_is_usage_error_without_http() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server),
        Command::RejectRemedy(RejectRemedyArgs {
            task_id: "bad".into(),
        }),
    )
    .await
    .expect_err("bad uuid");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn reject_remedy_dry_run_makes_no_request() {
    let server = MockServer::start().await;
    let mut ctx = ctx_for(&server);
    ctx.dry_run = true;
    let v = envelope_value(&run(&ctx, reject()).await.expect("dry-run"));
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_reject_remedy");
    assert!(server.received_requests().await.unwrap().is_empty());
}

const REFUSALS_409: [&str; 5] = [
    "onchain_dispute_disabled",
    "remedy_not_rejectable",
    "dispute_not_awaiting_signature",
    "escrow_already_disputed",
    "escrow_not_held",
];

async fn refuse(verb: &str, route: &str, status: u16, code: &str, cmd: Command) -> CmdError {
    let server = MockServer::start().await;
    Mock::given(method(verb))
        .and(path(format!("/tasks/{TASK_ID}/dispute/{route}")))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({
            "error": code,
            "message": "server says no",
        })))
        .mount(&server)
        .await;
    run(&ctx_for(&server), cmd).await.expect_err("refusal")
}

fn assert_409_code_and_hint(err: CmdError, code: &str) {
    match err {
        CmdError::Validation { code: c, message } => {
            assert_eq!(c, code);
            // A one-line action hint is added; the server's own text stays.
            assert!(message.contains("server says no"), "{message}");
            assert!(
                message.contains("→"),
                "no action hint for {code}: {message}"
            );
        }
        other => panic!("expected Validation for {code}, got {other:?}"),
    }
}

#[tokio::test]
async fn reject_remedy_409_codes_keep_code_and_add_hint() {
    for code in REFUSALS_409 {
        let err = refuse("POST", "reject-remedy", 409, code, reject()).await;
        assert_409_code_and_hint(err, code);
    }
}

#[tokio::test]
async fn reject_remedy_503_names_escrow_target_unavailable() {
    let err = refuse(
        "POST",
        "reject-remedy",
        503,
        "escrow_target_unavailable",
        reject(),
    )
    .await;
    match err {
        CmdError::Server(m) => {
            assert!(
                m.contains("→ hint: on this endpoint a 503 normally means"),
                "{m}"
            );
        }
        other => panic!("expected Server, got {other:?}"),
    }
}

// ─── sign ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn sign_dry_run_builds_dispute_calldata_for_the_server_contract() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_params(
        &server,
        params_json(
            Some(format!("{:#x}", keys.address)),
            CHAIN_ID,
            false,
            CONTRACT,
        ),
    )
    .await;
    mount_network(&server, CHAIN_ID).await;

    let v = envelope_value(&dry_sign(&server, &keys).await.expect("dry-run"));
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_submit_dispute");
    assert_eq!(v["data"]["to"], CONTRACT);
    assert_eq!(v["data"]["chain_id"], CHAIN_ID);
    assert_eq!(v["data"]["escrow_id"], ESCROW_ID);
    // Golden: `cast calldata "dispute(bytes32)" <ESCROW_ID>`.
    assert_eq!(
        v["data"]["calldata"],
        format!("0xadd98c70{}", &ESCROW_ID[2..])
    );
    assert_eq!(
        requests_to(&server, "/rpc").await,
        0,
        "dry-run must not touch RPC"
    );
}

#[tokio::test]
async fn sign_refuses_when_wallet_differs_from_poster_wallet_address() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let other = format!("{:#x}", PrivateKeySigner::random().address());
    mount_params(&server, params_json(Some(other), CHAIN_ID, false, CONTRACT)).await;
    mount_network(&server, CHAIN_ID).await;
    let err = dry_sign(&server, &keys).await.expect_err("wallet mismatch");
    assert!(
        matches!(&err, CmdError::Usage(m) if m.contains("poster_wallet_address")),
        "got {err:?}"
    );
}

#[tokio::test]
async fn sign_with_nil_poster_wallet_proceeds_with_a_warning() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_params(&server, params_json(None, CHAIN_ID, false, CONTRACT)).await;
    mount_network(&server, CHAIN_ID).await;
    let env = dry_sign(&server, &keys).await.expect("nil wallet signs");
    let v = envelope_value(&env);
    let warnings = v["security_warnings"].to_string();
    assert!(warnings.contains("poster_wallet_unknown"), "{warnings}");
}

#[tokio::test]
async fn sign_refuses_when_chain_id_differs_from_configured_network() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_params(
        &server,
        params_json(Some(format!("{:#x}", keys.address)), 1, false, CONTRACT),
    )
    .await;
    mount_network(&server, CHAIN_ID).await;
    let err = dry_sign(&server, &keys).await.expect_err("chain mismatch");
    assert!(
        matches!(&err, CmdError::Validation { code, .. } if code == "chain_id_mismatch"),
        "got {err:?}"
    );
}

#[tokio::test]
async fn sign_refuses_when_node_id_id_differs_before_any_send() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_params(
        &server,
        params_json(
            Some(format!("{:#x}", keys.address)),
            CHAIN_ID,
            false,
            CONTRACT,
        ),
    )
    .await;
    mount_network(&server, CHAIN_ID).await;
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": "0x1" })),
        )
        .mount(&server)
        .await;
    let rpc = format!("{}/rpc", server.uri());
    let err = run(&ctx_for(&server), Command::Sign(sign_args(&keys, &rpc)))
        .await
        .expect_err("node chain mismatch");
    assert!(
        matches!(&err, CmdError::Validation { code, .. } if code == "chain_id_mismatch"),
        "got {err:?}"
    );
    for r in server.received_requests().await.unwrap() {
        if r.url.path() == "/rpc" {
            let body = String::from_utf8_lossy(&r.body);
            assert!(!body.contains("sendRawTransaction"), "{body}");
        }
    }
}

#[tokio::test]
async fn sign_refuses_zero_escrow_contract() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let zero = "0x0000000000000000000000000000000000000000";
    mount_params(
        &server,
        params_json(Some(format!("{:#x}", keys.address)), CHAIN_ID, false, zero),
    )
    .await;
    mount_network(&server, CHAIN_ID).await;
    let err = dry_sign(&server, &keys).await.expect_err("zero contract");
    assert!(matches!(err, CmdError::Decode(_)), "got {err:?}");
}

#[tokio::test]
async fn sign_refuses_keystore_that_differs_from_wallet_address_flag() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_params(
        &server,
        params_json(
            Some(format!("{:#x}", keys.address)),
            CHAIN_ID,
            false,
            CONTRACT,
        ),
    )
    .await;
    mount_network(&server, CHAIN_ID).await;
    let mut args = sign_args(&keys, "http://rpc.invalid");
    args.wallet_address = Some(format!("{:#x}", PrivateKeySigner::random().address()));
    let mut ctx = ctx_for(&server);
    ctx.dry_run = true;
    let err = run(&ctx, Command::Sign(args)).await.expect_err("mismatch");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn sign_lapse_pending_still_proceeds_and_is_reported() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_params(
        &server,
        params_json(
            Some(format!("{:#x}", keys.address)),
            CHAIN_ID,
            true,
            CONTRACT,
        ),
    )
    .await;
    mount_network(&server, CHAIN_ID).await;
    let v = envelope_value(&dry_sign(&server, &keys).await.expect("lapse_pending signs"));
    assert_eq!(v["data"]["lapse_pending"], true);
    assert_eq!(v["data"]["escalation_deadline"], DEADLINE);
    assert!(v["security_warnings"].to_string().contains("lapse_pending"));
}

#[tokio::test]
async fn sign_params_409_and_503_keep_server_codes() {
    for code in REFUSALS_409 {
        let server = MockServer::start().await;
        let keys = fresh_keys();
        Mock::given(method("GET"))
            .and(path(format!("/tasks/{TASK_ID}/dispute/signing-params")))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "error": code, "message": "server says no",
            })))
            .mount(&server)
            .await;
        let err = dry_sign(&server, &keys).await.expect_err("409");
        assert_409_code_and_hint(err, code);
        assert_eq!(requests_to(&server, "/rpc").await, 0);
    }
    let server = MockServer::start().await;
    let keys = fresh_keys();
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}/dispute/signing-params")))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": "escrow_target_unavailable", "message": "server says no",
        })))
        .mount(&server)
        .await;
    let err = dry_sign(&server, &keys).await.expect_err("503");
    assert!(
        matches!(&err, CmdError::Server(m) if m.contains("escrow_target_unavailable")),
        "got {err:?}"
    );
}

#[tokio::test]
async fn sign_output_says_submitted_not_disputed() {
    // The dispute is final only once the reconciler sees EscrowDisputed.
    let server = MockServer::start().await;
    let keys = fresh_keys();
    mount_params(
        &server,
        params_json(
            Some(format!("{:#x}", keys.address)),
            CHAIN_ID,
            false,
            CONTRACT,
        ),
    )
    .await;
    mount_network(&server, CHAIN_ID).await;
    let v = envelope_value(&dry_sign(&server, &keys).await.expect("dry-run"));
    let text = v.to_string();
    assert!(!text.contains("\"disputed\""), "{text}");
    assert!(text.contains("would_submit_dispute"), "{text}");
}

// ─── timeout-resolve ──────────────────────────────────────────────────────

const DISPUTED_AT: u64 = 1_800_000_000;
const TIMEOUT: u64 = 10_368_000; // 120 days, served by the mock like the contract would
const STATE_DISPUTED: u8 = 5;

/// Answers the JSON-RPC calls `timeout-resolve` makes, from fixed chain facts.
struct Chain {
    node_id: u64,
    state: u8,
    block_ts: u64,
}

fn word(n: u64) -> String {
    format!("0x{n:064x}")
}

impl wiremock::Respond for Chain {
    fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let result = match body["method"].as_str().unwrap() {
            "eth_chainId" => json!(format!("0x{:x}", self.node_id)),
            "eth_getBlockByNumber" => json!({ "timestamp": format!("0x{:x}", self.block_ts) }),
            "eth_call" => {
                let data = body["params"][0]["data"].as_str().unwrap();
                if data.len() == 10 {
                    json!(word(TIMEOUT)) // DISPUTE_TIMEOUT(): selector only, no args
                } else if data.starts_with(&hex_selector(
                    &taskfast_agent::chain::disputed_at_calldata(ESCROW_ID.parse().unwrap()),
                )) {
                    json!(word(DISPUTED_AT))
                } else {
                    // getEscrow: 8 words, `state` last.
                    json!(format!("0x{}{:064x}", "00".repeat(32 * 7), self.state))
                }
            }
            m => panic!("unexpected rpc method {m}"),
        };
        ResponseTemplate::new(200)
            .set_body_json(json!({ "jsonrpc": "2.0", "id": 1, "result": result }))
    }
}

fn hex_selector(calldata: &[u8]) -> String {
    format!("0x{}", hex::encode(&calldata[..4]))
}

fn task_json(escrow_id: Option<&str>, contract: &str, chain_id: i64) -> Value {
    json!({
        "id": TASK_ID,
        "status": "disputed",
        "escrow_id": escrow_id,
        "settlement_domain": { "chain_id": chain_id, "verifying_contract": contract },
    })
}

async fn mount_task(server: &MockServer, body: Value) {
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn mount_chain(server: &MockServer, chain: Chain) {
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .respond_with(chain)
        .mount(server)
        .await;
}

fn tr_args(keys: &Keys, server: &MockServer) -> TimeoutResolveArgs {
    TimeoutResolveArgs {
        task_id: TASK_ID.into(),
        keystore: Some(keys.keystore_path.display().to_string()),
        wallet_password_file: Some(keys.password_path.clone()),
        wallet_address: None,
        rpc_url: Some(format!("{}/rpc", server.uri())),
        receipt_timeout: None,
    }
}

async fn tr(server: &MockServer, keys: &Keys, dry_run: bool) -> Result<Envelope, CmdError> {
    let mut ctx = ctx_for(server);
    ctx.dry_run = dry_run;
    run(&ctx, Command::TimeoutResolve(tr_args(keys, server))).await
}

async fn assert_nothing_sent(server: &MockServer) {
    for r in server.received_requests().await.unwrap() {
        if r.url.path() == "/rpc" {
            let body = String::from_utf8_lossy(&r.body);
            assert!(!body.contains("sendRawTransaction"), "{body}");
            assert!(
                !body.contains("estimateGas") && !body.contains("getTransactionCount"),
                "{body}"
            );
        }
    }
}

fn code_of(err: &CmdError) -> &str {
    match err {
        CmdError::Validation { code, .. } => code,
        other => panic!("expected Validation, got {other:?}"),
    }
}

async fn ready(chain: Chain) -> (MockServer, Keys) {
    let server = MockServer::start().await;
    mount_task(&server, task_json(Some(ESCROW_ID), CONTRACT, CHAIN_ID)).await;
    mount_network(&server, CHAIN_ID).await;
    mount_chain(&server, chain).await;
    (server, fresh_keys())
}

const ELIGIBLE: u64 = DISPUTED_AT + TIMEOUT;

#[tokio::test]
async fn timeout_resolve_refuses_one_second_before_the_deadline() {
    let (server, keys) = ready(Chain {
        node_id: CHAIN_ID as u64,
        state: STATE_DISPUTED,
        block_ts: ELIGIBLE - 1,
    })
    .await;
    let err = tr(&server, &keys, false).await.expect_err("too early");
    assert_eq!(code_of(&err), "dispute_timeout_not_elapsed");
    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn timeout_resolve_builds_the_call_at_exactly_the_deadline() {
    let (server, keys) = ready(Chain {
        node_id: CHAIN_ID as u64,
        state: STATE_DISPUTED,
        block_ts: ELIGIBLE,
    })
    .await;
    let v = envelope_value(&tr(&server, &keys, true).await.expect("dry-run"));
    assert_eq!(v["data"]["action"], "would_submit_timeout_resolve");
    // The task's settlement_domain contract is what we address.
    assert_eq!(v["data"]["to"], CONTRACT);
    assert_eq!(v["data"]["from"], format!("{:#x}", keys.address));
    assert_eq!(v["data"]["chain_id"], CHAIN_ID);
    // Golden: `cast calldata "timeoutResolveDispute(bytes32)" <ESCROW_ID>`.
    let selector = hex_selector(&taskfast_agent::chain::timeout_resolve_calldata(
        ESCROW_ID.parse().unwrap(),
    ));
    assert_eq!(
        v["data"]["calldata"],
        format!("{selector}{}", &ESCROW_ID[2..])
    );
    assert_eq!(v["data"]["eligible_at"], ELIGIBLE.to_string());
    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn timeout_resolve_judges_by_chain_time_not_the_local_clock() {
    // Block time is far before the deadline while the real clock is long past it.
    let (server, keys) = ready(Chain {
        node_id: CHAIN_ID as u64,
        state: STATE_DISPUTED,
        block_ts: 1,
    })
    .await;
    let err = tr(&server, &keys, true)
        .await
        .expect_err("chain clock behind");
    assert_eq!(code_of(&err), "dispute_timeout_not_elapsed");
}

#[tokio::test]
async fn timeout_resolve_refuses_every_non_disputed_state() {
    for state in [0u8, 1, 2, 3, 4] {
        let (server, keys) = ready(Chain {
            node_id: CHAIN_ID as u64,
            state,
            block_ts: ELIGIBLE + 1,
        })
        .await;
        let err = tr(&server, &keys, false).await.expect_err("not disputed");
        assert_eq!(code_of(&err), "escrow_not_disputed", "state {state}");
        assert_nothing_sent(&server).await;
    }
}

#[tokio::test]
async fn timeout_resolve_refuses_when_task_chain_differs_from_configured_network() {
    let server = MockServer::start().await;
    mount_task(&server, task_json(Some(ESCROW_ID), CONTRACT, 1)).await;
    mount_network(&server, CHAIN_ID).await;
    mount_chain(
        &server,
        Chain {
            node_id: 1,
            state: STATE_DISPUTED,
            block_ts: ELIGIBLE,
        },
    )
    .await;
    let keys = fresh_keys();
    let err = tr(&server, &keys, false)
        .await
        .expect_err("config chain mismatch");
    assert_eq!(code_of(&err), "chain_id_mismatch");
    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn timeout_resolve_refuses_when_node_id_differs() {
    let (server, keys) = ready(Chain {
        node_id: 1,
        state: STATE_DISPUTED,
        block_ts: ELIGIBLE,
    })
    .await;
    let err = tr(&server, &keys, false)
        .await
        .expect_err("node chain mismatch");
    assert_eq!(code_of(&err), "chain_id_mismatch");
    assert_nothing_sent(&server).await;
}

#[tokio::test]
async fn timeout_resolve_refuses_a_null_escrow_id_without_touching_the_chain() {
    let server = MockServer::start().await;
    mount_task(&server, task_json(None, CONTRACT, CHAIN_ID)).await;
    let err = tr(&server, &fresh_keys(), false)
        .await
        .expect_err("no escrow");
    assert_eq!(code_of(&err), "escrow_not_initialized");
    assert_eq!(requests_to(&server, "/rpc").await, 0);
}

#[tokio::test]
async fn timeout_resolve_never_falls_back_to_the_readiness_contract() {
    // No per-task domain: refuse rather than guess the (L1) readiness contract.
    let server = MockServer::start().await;
    mount_task(&server, json!({ "id": TASK_ID, "status": "disputed", "escrow_id": ESCROW_ID, "settlement_domain": null })).await;
    let err = tr(&server, &fresh_keys(), false)
        .await
        .expect_err("no domain");
    assert_eq!(code_of(&err), "settlement_domain_unavailable");
    assert_eq!(requests_to(&server, "/agents/me/readiness").await, 0);
    assert_eq!(requests_to(&server, "/rpc").await, 0);
}

#[tokio::test]
async fn timeout_resolve_refuses_zero_contract_and_bad_task_id() {
    let server = MockServer::start().await;
    mount_task(
        &server,
        task_json(
            Some(ESCROW_ID),
            "0x0000000000000000000000000000000000000000",
            CHAIN_ID,
        ),
    )
    .await;
    let err = tr(&server, &fresh_keys(), false)
        .await
        .expect_err("zero contract");
    assert!(matches!(err, CmdError::Decode(_)), "got {err:?}");
    let mut a = tr_args(&fresh_keys(), &server);
    a.task_id = "nope".into();
    let err = run(&ctx_for(&server), Command::TimeoutResolve(a))
        .await
        .expect_err("bad id");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}
