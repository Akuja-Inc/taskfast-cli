// SPDX-License-Identifier: MIT
//! End-to-end tests for `taskfast backer` (gh#54 Stream B, server #483).
//!
//! Stands up a wiremock server, drives `cmd::backer::run` directly, and asserts
//! on the JSON envelope. Backer management is owning-user only, so these also
//! assert the user PAT is sent in `X-API-Key`. Mirrors `tests/bid.rs`.

use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::backer::{run, AddArgs, Command, ListArgs, RevokeArgs};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const OPERATOR_ID: &str = "00000000-0000-0000-0000-0000000000a1";
const BACKER_ID: &str = "00000000-0000-0000-0000-0000000000b2";
const ACCOUNT_ID: &str = "00000000-0000-0000-0000-0000000000c3";
const WALLET: &str = "0x71C7656EC7ab88b098defB751B7401B5f6d8976F";
const PAT: &str = "tf_user_secrettoken";

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

fn backer_body(status: &str) -> serde_json::Value {
    json!({
        "id": BACKER_ID,
        "operator_id": OPERATOR_ID,
        "backer_account_id": ACCOUNT_ID,
        "backer_wallet_address": WALLET,
        "status": status,
    })
}

#[tokio::test]
async fn list_happy_path_returns_backers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/operators/{OPERATOR_ID}/backers")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "backers": [backer_body("active")],
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        operator: OPERATOR_ID.into(),
        human_api_key: None,
    };
    let envelope = run(&ctx_for(&server, Some(PAT)), Command::List(args))
        .await
        .expect("list ok");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["backers"][0]["status"], "active");
    assert_eq!(v["data"]["backers"][0]["backer_account_id"], ACCOUNT_ID);
}

#[tokio::test]
async fn add_happy_path_posts_account_and_wallet() {
    let server = MockServer::start().await;
    // header matcher proves the user PAT (not an agent key) is sent.
    Mock::given(method("POST"))
        .and(path(format!("/operators/{OPERATOR_ID}/backers")))
        .and(header("x-api-key", PAT))
        .and(body_partial_json(json!({
            "backer_account_id": ACCOUNT_ID,
            "backer_wallet_address": WALLET,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(backer_body("active")))
        .mount(&server)
        .await;

    let args = AddArgs {
        operator: OPERATOR_ID.into(),
        account: ACCOUNT_ID.into(),
        wallet: WALLET.into(),
        human_api_key: Some(PAT.into()),
    };
    let envelope = run(&ctx_for(&server, None), Command::Add(args))
        .await
        .expect("add ok");

    assert_eq!(
        envelope_value(&envelope)["data"]["backer"]["status"],
        "active"
    );
}

#[tokio::test]
async fn add_dry_run_skips_http() {
    let server = MockServer::start().await; // no mocks
    let mut ctx = ctx_for(&server, Some(PAT));
    ctx.dry_run = true;
    let args = AddArgs {
        operator: OPERATOR_ID.into(),
        account: ACCOUNT_ID.into(),
        wallet: WALLET.into(),
        human_api_key: None,
    };
    let v = envelope_value(&run(&ctx, Command::Add(args)).await.expect("dry-run ok"));
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["action"], "would_add_backer");
    assert_eq!(v["data"]["backer_account_id"], ACCOUNT_ID);
}

#[tokio::test]
async fn add_trims_wallet_whitespace() {
    // A copy-pasted address with trailing whitespace/newline must be trimmed
    // before it hits the wire, so the allowlist stores the canonical form.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/operators/{OPERATOR_ID}/backers")))
        .and(body_partial_json(
            json!({ "backer_wallet_address": WALLET }),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(backer_body("active")))
        .mount(&server)
        .await;

    let args = AddArgs {
        operator: OPERATOR_ID.into(),
        account: ACCOUNT_ID.into(),
        wallet: format!("  {WALLET}\n"),
        human_api_key: Some(PAT.into()),
    };
    run(&ctx_for(&server, None), Command::Add(args))
        .await
        .expect("padded wallet should be trimmed and accepted");
}

#[tokio::test]
async fn add_bad_operator_uuid_is_usage_error() {
    let server = MockServer::start().await;
    let args = AddArgs {
        operator: "nope".into(),
        account: ACCOUNT_ID.into(),
        wallet: WALLET.into(),
        human_api_key: Some(PAT.into()),
    };
    let err = run(&ctx_for(&server, None), Command::Add(args))
        .await
        .expect_err("bad operator uuid");
    match err {
        CmdError::Usage(m) => assert!(m.contains("--operator"), "unexpected: {m}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn add_empty_wallet_is_usage_error() {
    let server = MockServer::start().await;
    let args = AddArgs {
        operator: OPERATOR_ID.into(),
        account: ACCOUNT_ID.into(),
        wallet: "   ".into(),
        human_api_key: Some(PAT.into()),
    };
    let err = run(&ctx_for(&server, None), Command::Add(args))
        .await
        .expect_err("empty wallet");
    match err {
        CmdError::Usage(m) => assert!(m.contains("--wallet"), "unexpected: {m}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn revoke_happy_path_marks_revoked() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/operators/{OPERATOR_ID}/backers/{BACKER_ID}/revoke"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(backer_body("revoked")))
        .mount(&server)
        .await;

    let args = RevokeArgs {
        operator: OPERATOR_ID.into(),
        id: BACKER_ID.into(),
        human_api_key: Some(PAT.into()),
    };
    let envelope = run(&ctx_for(&server, None), Command::Revoke(args))
        .await
        .expect("revoke ok");
    assert_eq!(
        envelope_value(&envelope)["data"]["backer"]["status"],
        "revoked"
    );
}

#[tokio::test]
async fn revoke_bad_id_is_usage_error() {
    let server = MockServer::start().await;
    let args = RevokeArgs {
        operator: OPERATOR_ID.into(),
        id: "not-a-uuid".into(),
        human_api_key: Some(PAT.into()),
    };
    let err = run(&ctx_for(&server, None), Command::Revoke(args))
        .await
        .expect_err("bad backer id");
    match err {
        CmdError::Usage(m) => assert!(m.contains("--id"), "unexpected: {m}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn list_unauthorized_surfaces_as_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/operators/{OPERATOR_ID}/backers")))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "unauthorized",
            "message": "Authentication required",
        })))
        .mount(&server)
        .await;
    let args = ListArgs {
        operator: OPERATOR_ID.into(),
        human_api_key: Some(PAT.into()),
    };
    let err = run(&ctx_for(&server, None), Command::List(args))
        .await
        .expect_err("401");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn add_not_owner_403_surfaces_as_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/operators/{OPERATOR_ID}/backers")))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": "forbidden",
            "message": "not your operator",
        })))
        .mount(&server)
        .await;
    let args = AddArgs {
        operator: OPERATOR_ID.into(),
        account: ACCOUNT_ID.into(),
        wallet: WALLET.into(),
        human_api_key: Some(PAT.into()),
    };
    let err = run(&ctx_for(&server, None), Command::Add(args))
        .await
        .expect_err("403");
    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
}

#[tokio::test]
async fn add_invalid_backer_422_surfaces_as_validation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/operators/{OPERATOR_ID}/backers")))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "error": "invalid_backer_account",
            "message": "backer account must be a human account",
        })))
        .mount(&server)
        .await;
    let args = AddArgs {
        operator: OPERATOR_ID.into(),
        account: ACCOUNT_ID.into(),
        wallet: WALLET.into(),
        human_api_key: Some(PAT.into()),
    };
    let err = run(&ctx_for(&server, None), Command::Add(args))
        .await
        .expect_err("422");
    assert!(matches!(err, CmdError::Validation { .. }), "got {err:?}");
}

#[tokio::test]
async fn missing_credential_errors_before_any_http() {
    let server = MockServer::start().await;
    let args = ListArgs {
        operator: OPERATOR_ID.into(),
        human_api_key: None,
    };
    // no api_key and no human_api_key → MissingApiKey
    let err = run(&ctx_for(&server, None), Command::List(args))
        .await
        .expect_err("no credential");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}
