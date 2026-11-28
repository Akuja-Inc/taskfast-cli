// SPDX-License-Identifier: MIT
//! `task approve` signs a payout approval on a review-clock (v3) escrow and
//! `escrow payout-approval` retries it (taskfast#2044). wiremock only.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_chains::tempo::{distribution_digest, DistributionDomain};
use taskfast_cli::cmd::payout_approval::Args;
use taskfast_cli::cmd::task::{ApproveArgs, Command as TaskCommand};
use taskfast_cli::cmd::{escrow, task, CmdError, Ctx};
use taskfast_cli::exit::ExitCode;
use taskfast_cli::{Envelope, Environment};

const TASK_ID: &str = "00000000-0000-0000-0000-0000000000aa";
const CONTRACT: &str = "0x0000000000000000000000000000000000000001";
const ESCROW_ID: &str = "0xabababababababababababababababababababababababababababababababab";
const CHAIN_ID: i64 = 42_431;
const WINDOW: u64 = 86_400;
const HOUR: u64 = 3_600;

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

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
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

struct Chain {
    window: u64,
}

impl wiremock::Respond for Chain {
    fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let ok = |r: Value| {
            ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":1,"result":r}))
        };
        match body["method"].as_str().unwrap() {
            "eth_chainId" => ok(json!(format!("0x{:x}", CHAIN_ID))),
            "eth_call" => {
                let data = body["params"][0]["data"].as_str().unwrap();
                assert!(data.starts_with("0xec853c7d"), "unexpected call {data}");
                ok(json!(format!("0x{:064x}", self.window)))
            }
            m => panic!("unexpected rpc method {m}"),
        }
    }
}

async fn serve_task(server: &MockServer, status: &str, first_only: bool) {
    let mock = Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": TASK_ID,
            "status": status,
            "escrow_id": ESCROW_ID,
            "settlement_domain": {"chain_id": CHAIN_ID, "verifying_contract": CONTRACT},
        })));
    if first_only {
        mock.up_to_n_times(1).mount(server).await;
    } else {
        mock.mount(server).await;
    }
}

async fn serve(server: &MockServer, status: &str, window: u64) {
    serve_task(server, status, false).await;
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
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .respond_with(Chain { window })
        .mount(server)
        .await;
}

async fn mount_approve(server: &MockServer, status: &str) {
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/approve")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": TASK_ID,
            "status": status,
        })))
        .mount(server)
        .await;
}

async fn mount_payout(server: &MockServer, status: u16, body: Value) {
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/payout-approval")))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn payout_posts(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/payout-approval"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

async fn count_posts(server: &MockServer, suffix: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with(suffix))
        .count()
}

async fn rpc_calls(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/rpc")
        .count()
}

fn approve_args(keys: Option<&Keys>) -> ApproveArgs {
    ApproveArgs {
        id: TASK_ID.into(),
        keystore: keys.map(|k| k.keystore.display().to_string()),
        wallet_password_file: keys.map(|k| k.password.clone()),
        ..Default::default()
    }
}

async fn approve(server: &MockServer, keys: Option<&Keys>) -> Result<Envelope, CmdError> {
    task::run(&ctx_for(server), TaskCommand::Approve(approve_args(keys))).await
}

fn standalone_args(keys: Option<&Keys>) -> Args {
    Args {
        task_id: TASK_ID.into(),
        keystore: keys.map(|k| k.keystore.display().to_string()),
        wallet_password_file: keys.map(|k| k.password.clone()),
        wallet_address: None,
        rpc_url: None,
    }
}

async fn standalone(
    server: &MockServer,
    keys: Option<&Keys>,
    dry: bool,
) -> Result<Envelope, CmdError> {
    let mut ctx = ctx_for(server);
    ctx.dry_run = dry;
    escrow::run(&ctx, escrow::Command::PayoutApproval(standalone_args(keys))).await
}

fn signer_of(signature: &str, deadline: u64) -> Address {
    let domain = DistributionDomain::new(CHAIN_ID as u64, Address::from_str(CONTRACT).unwrap());
    let digest = distribution_digest(
        &domain,
        B256::from_str(ESCROW_ID).unwrap(),
        U256::from(deadline),
    );
    alloy_primitives::Signature::from_str(signature)
        .unwrap()
        .recover_address_from_prehash(&digest)
        .unwrap()
}

fn assert_in_server_window(deadline: u64) {
    let n = now();
    assert!(
        deadline >= n + 24 * HOUR + 60,
        "deadline too early: {deadline}"
    );
    assert!(
        deadline <= n + 48 * HOUR - 60,
        "deadline too late: {deadline}"
    );
}

#[tokio::test]
async fn v3_approve_signs_a_payout_approval_the_server_can_verify() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    serve(&server, "complete", WINDOW).await;
    mount_approve(&server, "complete").await;
    mount_payout(
        &server,
        200,
        json!({"task_id": TASK_ID, "poster_approval_deadline": 1}),
    )
    .await;

    let v = val(&approve(&server, Some(&keys)).await.expect("approve"));

    let posts = payout_posts(&server).await;
    assert_eq!(posts.len(), 1, "exactly one payout-approval POST");
    let deadline = posts[0]["deadline"].as_u64().unwrap();
    assert_in_server_window(deadline);
    let signature = posts[0]["signature"].as_str().unwrap();
    assert_eq!(signer_of(signature, deadline), keys.address);
    assert_eq!(
        v["data"]["payout_approval"]["action"],
        "payout_approval_recorded"
    );
    assert_eq!(v["data"]["status"], "complete");
}

#[tokio::test]
async fn v2_approve_loads_no_wallet_and_posts_no_approval() {
    let server = MockServer::start().await;
    serve(&server, "complete", 0).await;
    mount_approve(&server, "complete").await;

    let v = val(&approve(&server, None).await.expect("approve"));

    assert_eq!(v["data"]["payout_approval"]["action"], "no_review_clock");
    assert_eq!(count_posts(&server, "/payout-approval").await, 0);
}

#[tokio::test]
async fn v3_approve_without_a_wallet_exits_9_and_names_the_retry() {
    let server = MockServer::start().await;
    serve(&server, "complete", WINDOW).await;
    mount_approve(&server, "complete").await;

    let err = approve(&server, None).await.unwrap_err();

    assert!(
        matches!(&err, CmdError::ApprovedPayoutNotSigned(m)
            if m.contains(&format!("taskfast escrow payout-approval {TASK_ID}"))),
        "{err:?}"
    );
    assert_eq!(err.exit_code(), ExitCode::ApprovedPayoutNotSigned);
    assert_eq!(err.exit_code().as_u8(), 9);
    assert_eq!(count_posts(&server, "/approve").await, 1);
    assert_eq!(count_posts(&server, "/payout-approval").await, 0);
}

#[tokio::test]
async fn payout_approval_409_is_not_needed_not_an_error() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    serve_task(&server, "complete", true).await;
    serve_task(&server, "settled", false).await;
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
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/rpc"))
        .respond_with(Chain { window: WINDOW })
        .mount(&server)
        .await;
    mount_payout(
        &server,
        409,
        json!({"error": "payout_not_awaiting_approval", "message": "no new approval needed"}),
    )
    .await;

    let settled = val(&standalone(&server, Some(&keys), false).await.expect("409"));
    assert_eq!(settled["data"]["action"], "not_needed");
    assert_eq!(settled["data"]["note"], "already paid");

    let other = MockServer::start().await;
    serve(&other, "complete", WINDOW).await;
    mount_payout(
        &other,
        409,
        json!({"error": "payout_not_awaiting_approval", "message": "no new approval needed"}),
    )
    .await;
    let pending = val(&standalone(&other, Some(&keys), false).await.expect("409"));
    assert_eq!(pending["data"]["action"], "not_needed");
    assert_ne!(pending["data"]["note"], "already paid");
}

#[tokio::test]
async fn payout_approval_lifetime_refusal_says_check_the_clock() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    serve(&server, "complete", WINDOW).await;
    mount_payout(
        &server,
        422,
        json!({
            "error": "approval_lifetime_out_of_range",
            "message": "The approval deadline must be 24 to 48 hours from now",
            "min_seconds": 86_400,
            "max_seconds": 172_800,
        }),
    )
    .await;

    let err = standalone(&server, Some(&keys), false).await.unwrap_err();

    match &err {
        CmdError::Validation { code, message } => {
            assert_eq!(code, "approval_lifetime_out_of_range");
            assert!(message.contains("clock"), "{message}");
        }
        o => panic!("expected Validation, got {o:?}"),
    }
}

#[tokio::test]
async fn payout_approval_dry_run_signs_but_posts_nothing() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    serve(&server, "complete", WINDOW).await;

    let v = val(&standalone(&server, Some(&keys), true)
        .await
        .expect("dry run"));

    assert_eq!(v["data"]["action"], "would_record_payout_approval");
    let deadline = v["data"]["deadline"].as_u64().unwrap();
    assert_in_server_window(deadline);
    assert_eq!(
        signer_of(v["data"]["signature"].as_str().unwrap(), deadline),
        keys.address
    );
    assert_eq!(count_posts(&server, "/payout-approval").await, 0);
}

#[tokio::test]
async fn payout_approval_refuses_a_task_not_yet_approved() {
    let server = MockServer::start().await;
    let keys = fresh_keys();
    serve(&server, "under_review", WINDOW).await;

    let err = standalone(&server, Some(&keys), false).await.unwrap_err();

    assert!(
        matches!(&err, CmdError::Validation { code, .. } if code == "task_not_approved"),
        "{err:?}"
    );
    assert_eq!(rpc_calls(&server).await, 0);
    assert_eq!(count_posts(&server, "/payout-approval").await, 0);
}
