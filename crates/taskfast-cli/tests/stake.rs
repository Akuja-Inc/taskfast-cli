//! End-to-end tests for `taskfast stake` (gh#54, server #482).
//!
//! Each test stands up a wiremock server, drives `cmd::stake::run` directly,
//! and asserts on the JSON envelope shape. Mirrors `tests/bid.rs`.

use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::stake::{run, Args, StakeSource};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const TASK_ID: &str = "00000000-0000-0000-0000-0000000000aa";
const WALLET: &str = "0x71C7656EC7ab88b098defB751B7401B5f6d8976F";

fn ctx_for(server: &MockServer, key: Option<&str>) -> Ctx {
    Ctx {
        api_key: key.map(String::from),
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

fn args(source: StakeSource, wallet: Option<&str>) -> Args {
    Args {
        task_id: TASK_ID.into(),
        amount: 5_000_000,
        source,
        wallet: wallet.map(String::from),
    }
}

#[tokio::test]
async fn operator_self_happy_path_posts_amount_and_source() {
    let server = MockServer::start().await;
    // body_partial_json proves we forwarded amount + stake_source; a regression
    // that dropped either would slip past a status-code-only assertion.
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/stake")))
        .and(body_partial_json(
            json!({ "amount": 5_000_000, "stake_source": "operator_self" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": TASK_ID,
            "status": "assigned",
        })))
        .mount(&server)
        .await;

    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        args(StakeSource::OperatorSelf, None),
    )
    .await
    .expect("stake should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["stake"]["status"], "assigned");
    assert_eq!(v["data"]["stake"]["task_id"], TASK_ID);
}

#[tokio::test]
async fn posting_enabled_returns_awaiting_verification_without_task_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/stake")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "awaiting_verification",
        })))
        .mount(&server)
        .await;

    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        args(StakeSource::OperatorSelf, None),
    )
    .await
    .expect("stake should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["stake"]["status"], "awaiting_verification");
    assert!(v["data"]["stake"]["task_id"].is_null());
}

#[tokio::test]
async fn external_backer_forwards_wallet() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/stake")))
        .and(body_partial_json(json!({
            "amount": 5_000_000,
            "stake_source": "external_backer",
            "wallet_address": WALLET,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "task_id": TASK_ID,
            "status": "assigned",
        })))
        .mount(&server)
        .await;

    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        args(StakeSource::ExternalBacker, Some(WALLET)),
    )
    .await
    .expect("backer stake should succeed");

    assert_eq!(
        envelope_value(&envelope)["data"]["stake"]["status"],
        "assigned"
    );
}

#[tokio::test]
async fn dry_run_skips_http_and_echoes_request() {
    let server = MockServer::start().await; // no mocks mounted
    let mut ctx = ctx_for(&server, Some("test-key"));
    ctx.dry_run = true;

    let envelope = run(&ctx, args(StakeSource::OperatorSelf, None))
        .await
        .expect("dry-run ok");

    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_stake");
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["amount"], 5_000_000);
    assert_eq!(v["data"]["stake_source"], "operator_self");
}

#[tokio::test]
async fn bad_task_id_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let mut a = args(StakeSource::OperatorSelf, None);
    a.task_id = "not-a-uuid".into();
    let err = run(&ctx_for(&server, Some("test-key")), a)
        .await
        .expect_err("bad UUID must fail locally");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
}

#[tokio::test]
async fn nonpositive_amount_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let mut a = args(StakeSource::OperatorSelf, None);
    a.amount = 0;
    let err = run(&ctx_for(&server, Some("test-key")), a)
        .await
        .expect_err("amount < 1 must fail locally");
    match err {
        CmdError::Usage(m) => assert!(m.contains("--amount"), "unexpected: {m}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn external_backer_without_wallet_is_usage_error_without_any_http() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        args(StakeSource::ExternalBacker, None),
    )
    .await
    .expect_err("external-backer needs --wallet");
    match err {
        CmdError::Usage(m) => assert!(m.contains("--wallet"), "unexpected: {m}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn unauthorized_surfaces_as_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/stake")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        args(StakeSource::OperatorSelf, None),
    )
    .await
    .expect_err("401 must surface as Auth");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn not_operator_of_record_surfaces_as_auth_error() {
    // Per taskfast-cli error-mapping contract: 403 on a mutation is Auth.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/stake")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "not_operator_of_record",
            "message": "caller is not the operator of record",
        })))
        .mount(&server)
        .await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        args(StakeSource::OperatorSelf, None),
    )
    .await
    .expect_err("403 must surface as Auth");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn floor_unconfigured_409_surfaces() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/stake")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "high_assurance_floor_unconfigured",
            "message": "staking is not currently available",
        })))
        .mount(&server)
        .await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        args(StakeSource::OperatorSelf, None),
    )
    .await
    .expect_err("409 must surface");
    assert!(
        matches!(err, CmdError::Validation { .. } | CmdError::Server(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn below_floor_422_surfaces_as_validation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK_ID}/stake")))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "error": "stake_below_floor",
            "message": "amount is below the high-assurance minimum",
        })))
        .mount(&server)
        .await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        args(StakeSource::OperatorSelf, None),
    )
    .await
    .expect_err("422 must surface");
    assert!(matches!(err, CmdError::Validation { .. }), "got {err:?}");
}

#[tokio::test]
async fn missing_api_key_errors_before_any_http() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server, None),
        args(StakeSource::OperatorSelf, None),
    )
    .await
    .expect_err("no key → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}
