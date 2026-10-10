// SPDX-License-Identifier: MIT
//! End-to-end tests for `taskfast events` poll / ack / schema.
//!
//! Stands up a wiremock server, drives `cmd::events::run` directly, and
//! asserts on the JSON envelope shape + error mapping.

use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::events::{run, AckArgs, Command, PollArgs, SchemaArgs};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

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

#[tokio::test]
async fn poll_forwards_cursor_and_limit_and_returns_events() {
    let server = MockServer::start().await;
    let event = json!({
        "event_id": "00000000-0000-0000-0000-0000000000e1",
        "event": "task_disputed",
        "occurred_at": "2026-04-13T21:00:00Z",
        "task_id": "00000000-0000-0000-0000-0000000000aa",
        "data": { "reason": "late" },
    });
    Mock::given(method("GET"))
        .and(path("/agents/me/events"))
        .and(query_param("cursor", "abc"))
        .and(query_param("limit", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [event],
            "meta": { "next_cursor": "next-abc", "has_more": true, "total_count": 1 },
        })))
        .mount(&server)
        .await;

    let args = PollArgs {
        cursor: Some("abc".into()),
        limit: 3,
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::Poll(args))
        .await
        .expect("poll should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["meta"]["next_cursor"], "next-abc");
    assert_eq!(v["data"]["events"][0]["event"], "task_disputed");
}

#[tokio::test]
async fn poll_empty_page_returns_empty_events() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": { "next_cursor": null, "has_more": false, "total_count": 0 },
        })))
        .mount(&server)
        .await;

    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        Command::Poll(PollArgs {
            cursor: None,
            limit: 25,
        }),
    )
    .await
    .expect("poll should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["events"], json!([]));
    assert_eq!(v["data"]["meta"]["has_more"], false);
}

#[tokio::test]
async fn poll_401_surfaces_as_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/events"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;

    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Poll(PollArgs {
            cursor: None,
            limit: 25,
        }),
    )
    .await
    .expect_err("401 must surface as Auth");
    match err {
        CmdError::Auth(_) => {}
        other => panic!("expected Auth, got {other:?}"),
    }
}

#[tokio::test]
async fn poll_missing_api_key_errors_before_any_http_call() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server, None),
        Command::Poll(PollArgs {
            cursor: None,
            limit: 25,
        }),
    )
    .await
    .expect_err("no key → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}

/// gh#169: the server has no HTTP ack route, so `events ack` must fail
/// locally with a pointer to the stream's socket ack instead of POSTing.
#[tokio::test]
async fn ack_is_unsupported_and_points_to_ack_stdin() {
    let server = MockServer::start().await;
    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Ack(AckArgs {
            event_id: "00000000-0000-0000-0000-0000000000e1".into(),
        }),
    )
    .await
    .expect_err("ack → Usage");
    match err {
        CmdError::Usage(msg) => assert!(msg.contains("--ack-stdin"), "{msg}"),
        other => panic!("expected Usage, got {other:?}"),
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn schema_returns_full_spec_by_default() {
    let server = MockServer::start().await;
    let spec = json!({
        "asyncapi": "2.6.0",
        "info": { "title": "TaskFast Agent Events", "version": "1" },
        "components": {
            "messages": {
                "TaskAssigned": { "payload": { "type": "object" } },
                "BidAccepted":  { "payload": { "type": "object" } },
            }
        },
    });
    Mock::given(method("GET"))
        .and(path("/asyncapi.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(spec.clone()))
        .mount(&server)
        .await;

    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        Command::Schema(SchemaArgs { event: None }),
    )
    .await
    .expect("schema should succeed");
    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["asyncapi"], "2.6.0");
    assert!(v["data"]["components"]["messages"]["TaskAssigned"].is_object());
}

#[tokio::test]
async fn schema_filters_by_event_key() {
    let server = MockServer::start().await;
    let spec = json!({
        "asyncapi": "2.6.0",
        "components": {
            "messages": {
                "TaskAssigned": { "payload": { "type": "object", "required": ["task_id"] } },
            }
        },
    });
    Mock::given(method("GET"))
        .and(path("/asyncapi.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(spec))
        .mount(&server)
        .await;

    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        Command::Schema(SchemaArgs {
            event: Some("TaskAssigned".into()),
        }),
    )
    .await
    .expect("schema filter should succeed");
    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["event"], "TaskAssigned");
    assert_eq!(v["data"]["message"]["payload"]["required"][0], "task_id");
}

#[tokio::test]
async fn schema_unknown_event_is_validation_error() {
    let server = MockServer::start().await;
    let spec = json!({
        "asyncapi": "2.6.0",
        "components": { "messages": {} },
    });
    Mock::given(method("GET"))
        .and(path("/asyncapi.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(spec))
        .mount(&server)
        .await;

    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Schema(SchemaArgs {
            event: Some("MadeUpEvent".into()),
        }),
    )
    .await
    .expect_err("unknown event → Validation");
    match err {
        CmdError::Validation { code, .. } => assert_eq!(code, "unknown_event"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

/// Regression for the poll exit-6 bug: one malformed event must not
/// poison the whole page. Envelope carries a good event plus an
/// `unparseable` array entry instead of bailing on decode.
#[tokio::test]
async fn poll_tolerates_malformed_event_and_surfaces_unparseable() {
    let server = MockServer::start().await;
    let good = json!({
        "event_id": "00000000-0000-0000-0000-0000000000e1",
        "event": "task_disputed",
        "occurred_at": "2026-04-20T00:00:00Z",
        "data": {},
    });
    let bad = json!({
        "event": "task_disputed",
        "occurred_at": "2026-04-20T00:00:00Z",
        "data": {},
    });
    Mock::given(method("GET"))
        .and(path("/agents/me/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [good, bad],
            "meta": { "next_cursor": null, "has_more": false, "total_count": 2 },
        })))
        .mount(&server)
        .await;

    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        Command::Poll(PollArgs {
            cursor: None,
            limit: 25,
        }),
    )
    .await
    .expect("tolerant poll should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["events"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"]["events"][0]["event"], "task_disputed");
    assert_eq!(v["data"]["unparseable"].as_array().unwrap().len(), 1);
    assert!(v["data"]["unparseable"][0]["error"]
        .as_str()
        .unwrap()
        .contains("event_id"));
}
