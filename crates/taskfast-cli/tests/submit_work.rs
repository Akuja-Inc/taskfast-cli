// SPDX-License-Identifier: MIT
//! `escrow submit-work` and the automatic `submitWork` after `task submit` /
//! `task remedy` (taskfast#2044). wiremock serves the API; the chain is the
//! in-memory [`MockTempoRpc`] registered at the deployment's proxy URL, so the
//! real command signs with a throwaway key and broadcasts to it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use taskfast_agent::chain::get_escrow_calldata;
use taskfast_cli::cmd::{escrow, task, CmdError, Ctx};
use taskfast_cli::exit::ExitCode;
use taskfast_cli::tempo_session::{register_mock_rpc, MockTempoRpc};
use taskfast_cli::{Envelope, Environment};

const TASK_ID: &str = "00000000-0000-0000-0000-0000000000aa";
const CONTRACT: &str = "0x0000000000000000000000000000000000000001";
const ESCROW_ID: &str = "0xabababababababababababababababababababababababababababababababab";
const CHAIN_ID: i64 = 42_431;
const WINDOW: u64 = 86_400;
/// `review_deadline` of the current hand-in (2027-01-15T08:00:00Z).
const DEADLINE: u64 = 1_800_000_000;
const DEADLINE_ISO: &str = "2027-01-15T08:00:00Z";
const SELECTOR: &str = "3a9cfef2"; // submitWork(bytes32)
const REVIEW_WINDOW_OF: &str = "ec853c7d"; // reviewWindowOf(bytes32)
const SUBMITTED_AT: &str = "53fff8ac"; // submittedAt(bytes32)

fn selector(hex_sel: &str) -> [u8; 4] {
    hex::decode(hex_sel).unwrap().try_into().unwrap()
}

fn calldata_selector(calldata: &[u8]) -> [u8; 4] {
    calldata[..4].try_into().unwrap()
}

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

fn val(env: &Envelope) -> Value {
    serde_json::to_value(env).unwrap()
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

#[derive(Clone, Copy, PartialEq)]
enum Send {
    Ok,
    BroadcastFails,
}

/// Chain facts the node serves: `window` / `submitted_at` / `state` /
/// `worker` per selector.
struct Chain {
    window: u64,
    submitted_at: u64,
    state: u8,
    worker: Address,
    send: Send,
    /// Pre-review-clock contract: `reviewWindowOf` reverts.
    window_reverts: bool,
}

fn chain(keys: &Keys) -> Chain {
    Chain {
        window: WINDOW,
        submitted_at: 0,
        state: 1,
        worker: keys.address,
        send: Send::Ok,
        window_reverts: false,
    }
}

/// Programme the in-memory node with `c` and register it at the proxy URL the
/// mocked `/config/network` advertises.
fn mount_chain(server: &MockServer, c: Chain) -> Arc<MockTempoRpc> {
    let node = Arc::new(MockTempoRpc::new());
    node.set_chain_id(CHAIN_ID as u64);
    if c.window_reverts {
        node.set_call_revert(selector(REVIEW_WINDOW_OF));
    } else {
        node.set_call_result(
            selector(REVIEW_WINDOW_OF),
            hex::decode(word(c.window)).unwrap(),
        );
    }
    node.set_call_result(
        selector(SUBMITTED_AT),
        hex::decode(word(c.submitted_at)).unwrap(),
    );
    // getEscrow: poster, worker, platform, arbitrator, token, deposit, fee, state
    let get_escrow = format!(
        "{}{:064x}{}{}",
        word(0),
        U256::from_be_slice(c.worker.as_slice()),
        "00".repeat(32 * 5),
        word(u64::from(c.state))
    );
    node.set_call_result(
        calldata_selector(&get_escrow_calldata(ESCROW_ID.parse::<B256>().unwrap())),
        hex::decode(get_escrow).unwrap(),
    );
    if c.send == Send::BroadcastFails {
        node.reject_broadcast(selector(SELECTOR), "insufficient funds");
    }
    register_mock_rpc(&format!("{}/rpc", server.uri()), node.clone());
    node
}

async fn serve(server: &MockServer, status: &str, c: Chain) -> Arc<MockTempoRpc> {
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": TASK_ID,
            "status": status,
            "review_deadline": DEADLINE_ISO,
            "escrow_id": ESCROW_ID,
            "settlement_domain": {"chain_id": CHAIN_ID, "verifying_contract": CONTRACT},
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/config/network"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "networks": {"testnet": {
                "chain_id": CHAIN_ID,
                "rpc_url": format!("{}/rpc", server.uri()),
                "wss_url": null,
                "explorer_url": "https://explore.testnet.tempo.xyz",
            }}
        })))
        .mount(server)
        .await;
    mount_chain(server, c)
}

fn sw_args(keys: Option<&Keys>) -> escrow::SubmitWorkArgs {
    escrow::SubmitWorkArgs {
        task_id: TASK_ID.into(),
        keystore: keys.map(|k| k.keystore.display().to_string()),
        wallet_password_file: keys.map(|k| k.password.clone()),
        wallet_address: None,
        rpc_url: None,
        receipt_timeout: None,
    }
}

async fn sw(server: &MockServer, keys: Option<&Keys>, dry: bool) -> Result<Envelope, CmdError> {
    let mut ctx = ctx_for(server);
    ctx.dry_run = dry;
    escrow::run(&ctx, escrow::Command::SubmitWork(sw_args(keys))).await
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

fn sends(node: &MockTempoRpc) -> usize {
    node.sent_transactions().len()
}

fn code_of(e: &CmdError) -> &str {
    match e {
        CmdError::Validation { code, .. } => code,
        o => panic!("expected Validation, got {o:?}"),
    }
}

// ─── escrow submit-work: the behavior table ───────────────────────────────

#[tokio::test]
async fn legacy_escrow_sends_nothing_and_needs_no_wallet() {
    // Mutation: load the signer before reading the window → this errors (no keystore).
    let server = MockServer::start().await;
    let mut c = chain(&fresh_keys());
    c.window = 0;
    let node = serve(&server, "under_review", c).await;
    let v = val(&sw(&server, None, false).await.expect("legacy is success"));
    assert_eq!(v["data"]["action"], "no_review_clock");
    assert_eq!(sends(&node), 0);
}

#[tokio::test]
async fn first_hand_in_builds_submit_work_from_the_worker_wallet() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let node = serve(&server, "under_review", chain(&keys)).await;
    let v = val(&sw(&server, Some(&keys), true).await.expect("dry-run"));
    assert_eq!(v["data"]["action"], "would_submit_work");
    assert_eq!(v["data"]["to"], CONTRACT);
    assert_eq!(v["data"]["from"], format!("{:#x}", keys.address));
    assert_eq!(
        v["data"]["calldata"],
        format!("0x{SELECTOR}{}", &ESCROW_ID[2..])
    );
    assert_eq!(sends(&node), 0);
}

#[tokio::test]
async fn remedy_round_sends_again_when_the_recorded_hand_in_is_older() {
    // Mutation: skip whenever submittedAt != 0 → this would be `already_submitted`.
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mut c = chain(&keys);
    c.submitted_at = DEADLINE - WINDOW - 1; // earlier hand-in: + window is one second short
    serve(&server, "under_review", c).await;
    let v = val(&sw(&server, Some(&keys), true).await.unwrap());
    assert_eq!(v["data"]["action"], "would_submit_work");
}

#[tokio::test]
async fn same_hand_in_is_not_sent_twice() {
    // Mutation: `>` instead of `>=` in the duplicate rule → sends here.
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mut c = chain(&keys);
    c.submitted_at = DEADLINE - WINDOW;
    let node = serve(&server, "under_review", c).await;
    let v = val(&sw(&server, Some(&keys), false).await.unwrap());
    assert_eq!(v["data"]["action"], "already_submitted");
    assert_eq!(sends(&node), 0);
}

#[tokio::test]
async fn refuses_a_task_that_is_not_under_review() {
    for status in ["in_progress", "assigned", "complete", "disputed"] {
        let server = MockServer::start().await;
        let keys = fresh_keys();
        let node = serve(&server, status, chain(&keys)).await;
        let err = sw(&server, Some(&keys), false).await.unwrap_err();
        assert_eq!(code_of(&err), "task_not_under_review", "{status}");
        assert_eq!(sends(&node), 0);
    }
}

#[tokio::test]
async fn refuses_every_non_open_escrow_state() {
    for state in [0u8, 2, 3, 4, 5] {
        let server = MockServer::start().await;
        let keys = fresh_keys();
        let mut c = chain(&keys);
        c.state = state;
        let node = serve(&server, "under_review", c).await;
        let err = sw(&server, Some(&keys), false).await.unwrap_err();
        assert_eq!(code_of(&err), "escrow_not_open", "state {state}");
        assert_eq!(sends(&node), 0);
    }
}

#[tokio::test]
async fn refuses_a_wallet_that_is_not_the_escrow_worker() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mut c = chain(&keys);
    c.worker = fresh_keys().address;
    let node = serve(&server, "under_review", c).await;
    let err = sw(&server, Some(&keys), false).await.unwrap_err();
    assert_eq!(code_of(&err), "not_escrow_worker");
    assert_eq!(sends(&node), 0);
}

#[tokio::test]
async fn server_refusal_makes_no_rpc_call() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error":"forbidden"})))
        .mount(&server)
        .await;
    let keys = fresh_keys();
    let err = sw(&server, Some(&keys), false).await.unwrap_err();
    assert!(matches!(err, CmdError::Auth(_)), "{err:?}");
    assert_eq!(requests_to(&server, "/rpc").await, 0, "no chain call");
}

#[tokio::test]
async fn live_send_broadcasts_once_and_reports_the_tx() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let node = serve(&server, "under_review", chain(&keys)).await;
    let v = val(&sw(&server, Some(&keys), false).await.expect("send"));
    assert_eq!(v["data"]["action"], "submit_work_submitted");
    let tx_hash = v["data"]["tx_hash"].as_str().unwrap();
    assert!(
        tx_hash.starts_with("0x") && tx_hash.len() == 66,
        "{tx_hash}"
    );
    let sent = node.sent_transactions();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].0,
        CONTRACT.parse::<Address>().unwrap(),
        "targets the escrow"
    );
    assert_eq!(&sent[0].1[..4], selector(SELECTOR));
}

// ─── automatic send after `task submit` ───────────────────────────────────

fn submit_args(keys: Option<&Keys>) -> task::SubmitArgs {
    task::SubmitArgs {
        id: TASK_ID.into(),
        summary: "done".into(),
        keystore: keys.map(|k| k.keystore.display().to_string()),
        wallet_password_file: keys.map(|k| k.password.clone()),
        ..Default::default()
    }
}

async fn mount_submit(server: &MockServer, status: u16) {
    mount_hand_in(server, "submit", status, "under_review").await;
}

fn hand_in_response(status: u16, task_status: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({
        "success": status == 200,
        "task_id": TASK_ID,
        "status": task_status,
        "message": "submitted",
        "error": "invalid_status",
        "evaluation": {"passed": true, "criteria_results": [], "evaluated_at": "2026-04-13T21:00:00Z"},
    }))
}

async fn mount_hand_in(server: &MockServer, verb: &str, status: u16, task_status: &str) {
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/{verb}")))
        .respond_with(hand_in_response(status, task_status))
        .mount(server)
        .await;
}

async fn run_remedy(server: &MockServer, keys: Option<&Keys>) -> Result<Envelope, CmdError> {
    task::run(&ctx_for(server), task::Command::Remedy(submit_args(keys))).await
}

async fn run_submit(server: &MockServer, keys: Option<&Keys>) -> Result<Envelope, CmdError> {
    task::run(&ctx_for(server), task::Command::Submit(submit_args(keys))).await
}

#[tokio::test]
async fn server_refusal_means_no_chain_call_at_all() {
    // Mutation: send on chain before/without the server accepting → rpc is hit.
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let node = serve(&server, "under_review", chain(&keys)).await;
    mount_submit(&server, 409).await;
    let err = run_submit(&server, Some(&keys)).await.unwrap_err();
    assert!(
        !matches!(err, CmdError::HandedInClockNotStarted(_)),
        "{err:?}"
    );
    assert_eq!(node.request_count(), 0, "no chain call at all");
}

#[tokio::test]
async fn good_hand_in_then_chain_send_in_that_order() {
    // Mutation: drop the automatic send → sends == 0.
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let node = serve(&server, "under_review", chain(&keys)).await;
    // Record how many txs the node had seen when the hand-in arrived.
    let sends_at_hand_in = Arc::new(AtomicUsize::new(usize::MAX));
    {
        let node = node.clone();
        let sends_at_hand_in = sends_at_hand_in.clone();
        Mock::given(method("POST"))
            .and(path(format!("/tasks/{TASK_ID}/submit")))
            .respond_with(move |_: &Request| {
                sends_at_hand_in.store(node.sent_transactions().len(), Ordering::SeqCst);
                hand_in_response(200, "under_review")
            })
            .mount(&server)
            .await;
    }
    let v = val(&run_submit(&server, Some(&keys)).await.expect("submit"));
    assert_eq!(v["data"]["chain"]["action"], "submit_work_submitted");
    assert_eq!(v["data"]["submission"]["status"], "under_review");
    assert_eq!(sends_at_hand_in.load(Ordering::SeqCst), 0, "hand-in first");
    assert_eq!(sends(&node), 1, "then the send");
}

#[tokio::test]
async fn legacy_escrow_submit_prompts_for_no_wallet_and_sends_nothing() {
    let server = MockServer::start().await;
    let mut c = chain(&fresh_keys());
    c.window = 0;
    let node = serve(&server, "under_review", c).await;
    mount_submit(&server, 200).await;
    let v = val(&run_submit(&server, None).await.expect("legacy submit"));
    assert_eq!(v["data"]["chain"]["action"], "no_review_clock");
    assert_eq!(sends(&node), 0);
}

#[tokio::test]
async fn chain_failure_after_good_hand_in_is_not_a_failed_hand_in() {
    // Mutation: propagate the plain chain error → code/exit/message lose the "handed in" meaning.
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let mut c = chain(&keys);
    c.send = Send::BroadcastFails;
    let node = serve(&server, "under_review", c).await;
    mount_submit(&server, 200).await;
    let err = run_submit(&server, Some(&keys)).await.unwrap_err();
    assert_eq!(sends(&node), 1, "the node was asked and refused");
    assert!(
        matches!(err, CmdError::HandedInClockNotStarted(_)),
        "{err:?}"
    );
    assert_eq!(err.code(), "handed_in_clock_not_started");
    assert_eq!(err.exit_code(), ExitCode::HandedInClockNotStarted);
    assert_eq!(err.exit_code().as_u8(), 8);
    let msg = err.to_string();
    assert!(msg.contains("IS handed in"), "{msg}");
    assert!(msg.contains("did NOT start"), "{msg}");
    assert!(
        msg.contains(&format!("taskfast escrow submit-work {TASK_ID}")),
        "{msg}"
    );
    assert!(!msg.contains("task submit"), "{msg}");
    // The hand-in itself was posted exactly once.
    let posts = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/submit"))
        .count();
    assert_eq!(posts, 1);
}

#[tokio::test]
async fn missing_wallet_on_a_review_clock_escrow_reports_clock_not_started() {
    let server = MockServer::start().await;
    let node = serve(&server, "under_review", chain(&fresh_keys())).await;
    mount_submit(&server, 200).await;
    let err = run_submit(&server, None).await.unwrap_err();
    assert!(
        matches!(err, CmdError::HandedInClockNotStarted(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains("escrow submit-work"));
    assert_eq!(sends(&node), 0);
}

#[tokio::test]
async fn auto_approved_task_has_no_clock_to_start() {
    // Server already completed the task: not an error, nothing sent.
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let node = serve(&server, "complete", chain(&keys)).await;
    mount_submit(&server, 200).await;
    let v = val(&run_submit(&server, Some(&keys)).await.expect("submit"));
    assert_eq!(v["data"]["chain"]["action"], "skipped");
    assert_eq!(sends(&node), 0);
}

// ─── review-fix cases ─────────────────────────────────────────────────────

#[tokio::test]
async fn pre_review_clock_contract_revert_is_a_legacy_escrow() {
    // `reviewWindowOf` does not exist on a pre-v3 contract: the call reverts.
    // Mutation: treat any read error as exit 8 → this fails on every legacy submit.
    let server = MockServer::start().await;
    let mut c = chain(&fresh_keys());
    c.window_reverts = true;
    let node = serve(&server, "under_review", c).await;
    mount_submit(&server, 200).await;
    let v = val(&run_submit(&server, None).await.expect("legacy submit"));
    assert_eq!(v["data"]["chain"]["action"], "no_review_clock");
    assert_eq!(sends(&node), 0);
}

#[tokio::test]
async fn remedy_also_starts_the_clock() {
    // Mutation: send only for `task submit` → remedy sends nothing.
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let node = serve(&server, "under_review", chain(&keys)).await;
    mount_hand_in(&server, "remedy", 200, "under_review").await;
    let v = val(&run_remedy(&server, Some(&keys)).await.expect("remedy"));
    assert_eq!(v["data"]["chain"]["action"], "submit_work_submitted");
    assert_eq!(sends(&node), 1);
}

#[tokio::test]
async fn remedy_that_ends_the_task_starts_no_clock() {
    for end in ["complete", "cancelled"] {
        let server = MockServer::start().await;
        let keys = fresh_keys();
        let node = serve(&server, end, chain(&keys)).await;
        mount_hand_in(&server, "remedy", 200, end).await;
        let v = val(&run_remedy(&server, Some(&keys)).await.expect("remedy"));
        assert_eq!(v["data"]["chain"]["action"], "skipped", "{end}");
        assert_eq!(node.request_count(), 0, "no chain call, {end}");
    }
}

#[tokio::test]
async fn missing_password_on_a_review_clock_escrow_reports_clock_not_started() {
    // Keystore set, no password file and no env password.
    let server = MockServer::start().await;
    let keys = fresh_keys();
    let node = serve(&server, "under_review", chain(&keys)).await;
    mount_submit(&server, 200).await;
    let mut args = submit_args(Some(&keys));
    args.wallet_password_file = None;
    let err = task::run(&ctx_for(&server), task::Command::Submit(args))
        .await
        .unwrap_err();
    assert!(
        matches!(err, CmdError::HandedInClockNotStarted(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains("escrow submit-work"));
    assert_eq!(sends(&node), 0);
}

#[tokio::test]
async fn escrow_without_a_settlement_domain_is_not_silently_skipped() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": TASK_ID, "status": "under_review", "escrow_id": ESCROW_ID,
        })))
        .mount(&server)
        .await;
    mount_submit(&server, 200).await;
    let err = run_submit(&server, None).await.unwrap_err();
    assert!(
        matches!(err, CmdError::HandedInClockNotStarted(_)),
        "{err:?}"
    );
}
