//! End-to-end tests for `taskfast bid` read path (list).
//!
//! Each test stands up a wiremock server, drives `cmd::bid::run` directly,
//! and asserts on the JSON envelope shape.

use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::bid::{Command, ListArgs, run};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Envelope, Environment};

const BID_ID: &str = "00000000-0000-0000-0000-00000000b1d1";
const TASK_ID: &str = "00000000-0000-0000-0000-0000000000aa";

fn ctx_for(server: &MockServer, key: Option<&str>) -> Ctx {
    Ctx {
        api_key: key.map(String::from),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        dry_run: false,
        quiet: true,
    }
}

fn envelope_value(env: &Envelope) -> serde_json::Value {
    serde_json::to_value(env).expect("serialize envelope")
}

fn paginated(cursor: Option<&str>) -> serde_json::Value {
    match cursor {
        Some(c) => json!({ "next_cursor": c, "has_more": true, "total_count": 1 }),
        None => json!({ "next_cursor": null, "has_more": false, "total_count": 0 }),
    }
}

#[tokio::test]
async fn list_forwards_cursor_and_limit_and_returns_bids() {
    let server = MockServer::start().await;
    let bid = json!({
        "id": BID_ID,
        "task_id": TASK_ID,
        "agent_id": "00000000-0000-0000-0000-0000000000a0",
        "price": "100.00",
        "status": "pending",
        "created_at": "2026-04-13T21:00:00Z",
    });
    Mock::given(method("GET"))
        .and(path("/api/agents/me/bids"))
        .and(query_param("cursor", "abc"))
        .and(query_param("limit", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [bid],
            "meta": paginated(Some("next-abc")),
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        cursor: Some("abc".into()),
        limit: Some(5),
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["meta"]["next_cursor"], "next-abc");
    assert_eq!(v["data"]["bids"][0]["id"], BID_ID);
    assert_eq!(v["data"]["bids"][0]["price"], "100.00");
}

#[tokio::test]
async fn list_without_pagination_params_returns_empty() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/bids"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": paginated(None),
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        cursor: None,
        limit: None,
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["bids"], json!([]));
    assert_eq!(v["data"]["meta"]["has_more"], false);
}

#[tokio::test]
async fn list_401_surfaces_as_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/agents/me/bids"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        cursor: None,
        limit: None,
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect_err("401 must surface as Auth");
    match err {
        CmdError::Auth(_) => {}
        other => panic!("expected Auth, got {other:?}"),
    }
}

#[tokio::test]
async fn list_missing_api_key_errors_before_any_http_call() {
    let server = MockServer::start().await;
    let args = ListArgs {
        cursor: None,
        limit: None,
    };
    let err = run(&ctx_for(&server, None), Command::List(args))
        .await
        .expect_err("no key → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}

#[tokio::test]
async fn deferred_subcommands_return_unimplemented() {
    let server = MockServer::start().await;
    for cmd in [
        Command::Create {
            task_id: TASK_ID.into(),
            amount: "10.00".into(),
        },
        Command::Cancel {
            id: BID_ID.into(),
        },
        Command::Accept {
            id: BID_ID.into(),
        },
        Command::Reject {
            id: BID_ID.into(),
        },
    ] {
        let err = run(&ctx_for(&server, Some("test-key")), cmd)
            .await
            .expect_err("stubs must return Unimplemented");
        assert!(matches!(err, CmdError::Unimplemented(_)), "got {err:?}");
    }
}
