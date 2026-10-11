// SPDX-License-Identifier: MIT
//! End-to-end wiremock test for `taskfast me`.
//!
//! Exercises the full pipeline: Ctx → Ctx::client() → {get_agent_profile_raw,
//! get_readiness} → Envelope. This is the canary for the whole CLI stack;
//! if it breaks, every other subcommand will too.

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::me::{run, Args};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

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
    serde_json::to_value(env).expect("envelope serializes")
}

#[tokio::test]
async fn me_happy_path_returns_profile_readiness_envelope() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "00000000-0000-0000-0000-000000000042",
            "name": "alice",
            "status": "active",
            "capabilities": ["coding"],
            // gh#172: unknown to this CLI's spec; must pass through to data.profile.
            "account_id": "2b3c4d5e-6f7a-4b8c-9d0e-1f2a3b4c5d6e",
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ready_to_work": true,
            "checks": {
                "api_key": {"status": "complete"},
                "wallet": {"status": "complete"},
                "funded": {"status": "complete"},
                "webhook": {"status": "not_configured", "required": false},
            },
            "settlement_domain": {
                "chain_id": 42431,
                "verifying_contract": "0x0000000000000000000000000000000000000000",
            },
        })))
        .mount(&server)
        .await;

    let envelope = run(&ctx_for(&server), Args { resume: false })
        .await
        .expect("me should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["environment"], "local");
    assert_eq!(v["data"]["ready_to_work"], true);
    assert_eq!(v["data"]["profile"]["name"], "alice");
    assert_eq!(
        v["data"]["profile"]["account_id"],
        "2b3c4d5e-6f7a-4b8c-9d0e-1f2a3b4c5d6e"
    );
    assert_eq!(v["data"]["readiness"]["ready_to_work"], true);
    assert_eq!(
        v["data"]["readiness"]["checks"]["webhook"]["status"],
        "not_configured"
    );
}

#[tokio::test]
async fn me_preserves_unknown_agent_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "alice",
            "status": "suspended_pending_review",
            "capabilities": [],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ready_to_work": false,
            "checks": {
                "api_key": {"status": "complete"},
                "wallet": {"status": "missing"},
                "funded": {"status": "no_wallet"},
                "webhook": {"status": "not_configured", "required": false},
            },
            "settlement_domain": {
                "chain_id": 42431,
                "verifying_contract": "0x0000000000000000000000000000000000000000",
            },
        })))
        .mount(&server)
        .await;

    let envelope = run(&ctx_for(&server), Args { resume: false })
        .await
        .expect("unknown agent status must decode");
    assert_eq!(
        envelope_value(&envelope)["data"]["profile"]["status"],
        "suspended_pending_review"
    );
}

/// Mounts `/agents/me` and a readiness body, for the gh#178 tests below.
async fn mount_me_with_readiness(server: &MockServer, readiness: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "alice",
            "status": "active",
            "capabilities": [],
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(readiness))
        .mount(server)
        .await;
}

#[tokio::test]
async fn me_tolerates_a_missing_required_field_the_cli_does_not_read() {
    // `ready_to_work` is required in the spec but not in KEEP_REQUIRED (gh#178).
    let server = MockServer::start().await;
    mount_me_with_readiness(
        &server,
        json!({
            "checks": {
                "api_key": {"status": "complete"},
                "wallet": {"status": "missing"},
                "funded": {"status": "no_wallet"},
                "webhook": {"status": "not_configured", "required": false},
            },
            "settlement_domain": {
                "chain_id": 42431,
                "verifying_contract": "0x0000000000000000000000000000000000000000",
            },
        }),
    )
    .await;

    let envelope = run(&ctx_for(&server), Args { resume: false })
        .await
        .expect("a missing field the CLI does not read must decode");
    assert_eq!(envelope_value(&envelope)["ok"], true);
}

#[tokio::test]
async fn me_still_fails_on_a_missing_field_the_cli_reads() {
    // `settlement_domain` is in KEEP_REQUIRED: settle signs against it.
    let server = MockServer::start().await;
    mount_me_with_readiness(
        &server,
        json!({
            "ready_to_work": false,
            "checks": {
                "api_key": {"status": "complete"},
                "wallet": {"status": "missing"},
                "funded": {"status": "no_wallet"},
                "webhook": {"status": "not_configured", "required": false},
            },
        }),
    )
    .await;

    let err = run(&ctx_for(&server), Args { resume: false })
        .await
        .expect_err("a missing kept field must fail decode");
    assert_eq!(err.code(), "decode", "got {err:?}");
    assert!(
        format!("{err:?}").contains("settlement_domain"),
        "got {err:?}"
    );
}

#[tokio::test]
async fn me_surfaces_not_ready_verbatim() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "alice",
            "status": "active",
            "capabilities": [],
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ready_to_work": false,
            "checks": {
                "api_key": {"status": "complete"},
                "wallet": {
                    "status": "missing",
                    "hint": "POST /agents/me/wallet"
                },
                "funded": {"status": "no_wallet"},
                "webhook": {"status": "not_configured", "required": false},
            },
            "settlement_domain": {
                "chain_id": 42431,
                "verifying_contract": "0x0000000000000000000000000000000000000000",
            },
        })))
        .mount(&server)
        .await;

    let envelope = run(&ctx_for(&server), Args { resume: false })
        .await
        .expect("me should return success even if not ready");

    let v = envelope_value(&envelope);
    // `ok` reflects CLI success (we got data), NOT readiness — that's what
    // the duplicated `ready_to_work` field is for.
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["ready_to_work"], false);
    assert_eq!(
        v["data"]["readiness"]["checks"]["wallet"]["status"],
        "missing"
    );
    assert_eq!(
        v["data"]["readiness"]["checks"]["wallet"]["hint"],
        "POST /agents/me/wallet"
    );
}

#[tokio::test]
async fn me_maps_401_to_auth_error() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;

    let err = run(&ctx_for(&server), Args { resume: false })
        .await
        .expect_err("401 must surface");

    assert!(matches!(err, CmdError::Auth(_)), "got {err:?}");
    assert_eq!(err.code(), "auth");
}

#[tokio::test]
async fn me_without_api_key_errors_with_missing_api_key() {
    let ctx = Ctx {
        api_key: None,
        environment: Environment::Local,
        api_base: Some("http://unused".into()),
        config_path: std::path::PathBuf::from("/dev/null"),
        dry_run: false,
        quiet: true,
        ..Default::default()
    };
    let err = run(&ctx, Args { resume: false })
        .await
        .expect_err("no api key → missing_api_key");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
    assert_eq!(err.code(), "missing_api_key");
}

#[tokio::test]
async fn me_resume_is_unimplemented_for_now() {
    let server = MockServer::start().await;
    let err = run(&ctx_for(&server), Args { resume: true })
        .await
        .expect_err("--resume deferred");
    assert!(matches!(err, CmdError::Unimplemented(_)), "got {err:?}");
}
