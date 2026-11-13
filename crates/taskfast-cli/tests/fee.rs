// SPDX-License-Identifier: MIT
//! End-to-end tests for `taskfast fee retry` (the voucher-carrying successor
//! of `task retry-fee`) and for the old command's redirect.

use clap::Parser;
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::fee::{run, Command, RetryArgs};
use taskfast_cli::cmd::task::{self, IdOnlyArgs};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const TASK_ID: &str = "00000000-0000-0000-0000-0000000000aa";
const VOUCHER: &str = "0xdeadbeef";

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

fn retry(voucher: &str) -> Command {
    Command::Retry(RetryArgs {
        task_id: TASK_ID.into(),
        submission_fee_voucher: voucher.into(),
    })
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
    run(&ctx_for(&server), retry(VOUCHER))
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

    let env = run(&ctx_for(&server), retry(VOUCHER)).await.expect("ok");
    let v = envelope_value(&env);
    assert_eq!(v["data"]["task_id"], TASK_ID);
    assert_eq!(v["data"]["status"], "pending_evaluation");
}

#[tokio::test]
async fn fee_retry_without_voucher_flag_is_a_clap_error() {
    let err = RetryArgs::try_parse_from(["retry", TASK_ID]).expect_err("flag is required");
    assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
}

#[tokio::test]
async fn fee_retry_empty_voucher_is_usage_error_without_http() {
    let server = MockServer::start().await; // no mocks: any request would 404
    let err = run(&ctx_for(&server), retry("  "))
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
    let err = run(&ctx_for(&server), retry("not-hex"))
        .await
        .expect_err("non-hex");
    assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn fee_retry_dry_run_prints_task_and_voucher_without_http() {
    let server = MockServer::start().await;
    let mut ctx = ctx_for(&server);
    ctx.dry_run = true;
    let env = run(&ctx, retry(VOUCHER)).await.expect("dry-run");
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
            submission_fee_voucher: VOUCHER.into(),
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
        CmdError::Usage(m) => assert!(m.contains("taskfast fee retry"), "{m}"),
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
