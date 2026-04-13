//! End-to-end tests for `taskfast task` read path (list + get).
//!
//! Each test stands up a wiremock server, drives `cmd::task::run`
//! directly, and asserts on the JSON envelope shape.

use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::task::{Command, GetArgs, ListArgs, ListKind, TaskStatus, run};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::{Environment, Envelope};

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
        Some(c) => json!({ "next_cursor": c, "has_more": true, "total_count": 0 }),
        None => json!({ "next_cursor": null, "has_more": false, "total_count": 0 }),
    }
}

#[tokio::test]
async fn list_mine_forwards_status_and_cursor_and_returns_tasks() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/tasks"))
        .and(query_param("status", "in_progress"))
        .and(query_param("cursor", "abc"))
        .and(query_param("limit", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": paginated(Some("next-abc")),
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        kind: ListKind::Mine,
        status: Some(TaskStatus::InProgress),
        cursor: Some("abc".into()),
        limit: Some(5),
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list mine should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["kind"], "mine");
    assert_eq!(v["data"]["meta"]["next_cursor"], "next-abc");
    assert_eq!(v["data"]["tasks"], json!([]));
}

#[tokio::test]
async fn list_queue_hits_queue_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/queue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": paginated(None),
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        kind: ListKind::Queue,
        status: None,
        cursor: None,
        limit: None,
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list queue should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["kind"], "queue");
}

#[tokio::test]
async fn list_posted_hits_posted_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/posted_tasks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [],
            "meta": paginated(None),
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        kind: ListKind::Posted,
        status: None,
        cursor: None,
        limit: None,
    };
    let envelope = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect("list posted should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["kind"], "posted");
}

#[tokio::test]
async fn list_status_with_non_mine_kind_is_usage_error() {
    // No server hit expected — usage error fires before any HTTP call.
    let server = MockServer::start().await;
    let args = ListArgs {
        kind: ListKind::Queue,
        status: Some(TaskStatus::Assigned),
        cursor: None,
        limit: None,
    };
    let err = run(&ctx_for(&server, Some("test-key")), Command::List(args))
        .await
        .expect_err("status + non-mine kind must fail");
    match err {
        CmdError::Usage(msg) => assert!(msg.contains("--status"), "got: {msg}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn get_returns_task_detail() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": TASK_ID,
            "title": "test task",
            "status": "open",
            "description": "hello",
        })))
        .mount(&server)
        .await;

    let envelope = run(
        &ctx_for(&server, Some("test-key")),
        Command::Get(GetArgs {
            id: TASK_ID.into(),
        }),
    )
    .await
    .expect("get should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["task"]["id"], TASK_ID);
    assert_eq!(v["data"]["task"]["title"], "test task");
}

#[tokio::test]
async fn get_bad_uuid_is_usage_error_without_hitting_server() {
    let server = MockServer::start().await;
    // Deliberately no mock — a hit would 404 and fail the test.

    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Get(GetArgs {
            id: "not-a-uuid".into(),
        }),
    )
    .await
    .expect_err("bad uuid must error locally");
    match err {
        CmdError::Usage(msg) => assert!(msg.contains("UUID"), "got: {msg}"),
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn get_404_surfaces_as_validation_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/tasks/{TASK_ID}")))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": "task_not_found",
            "message": "no task with that id",
        })))
        .mount(&server)
        .await;

    let err = run(
        &ctx_for(&server, Some("test-key")),
        Command::Get(GetArgs {
            id: TASK_ID.into(),
        }),
    )
    .await
    .expect_err("404 must surface as Validation per client mapping");
    match err {
        CmdError::Validation { code, .. } => assert_eq!(code, "task_not_found"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn list_401_surfaces_as_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/tasks"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid_api_key",
            "message": "bad key",
        })))
        .mount(&server)
        .await;

    let args = ListArgs {
        kind: ListKind::Mine,
        status: None,
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
async fn missing_api_key_errors_before_any_http_call() {
    let server = MockServer::start().await;
    let args = ListArgs {
        kind: ListKind::Mine,
        status: None,
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
        Command::Submit {
            id: TASK_ID.into(),
            artifact: vec![],
            summary: "s".into(),
        },
        Command::Approve {
            id: TASK_ID.into(),
        },
        Command::Dispute {
            id: TASK_ID.into(),
            reason: "r".into(),
        },
        Command::Cancel {
            id: TASK_ID.into(),
        },
    ] {
        let err = run(&ctx_for(&server, Some("test-key")), cmd)
            .await
            .expect_err("stubs must return Unimplemented");
        assert!(matches!(err, CmdError::Unimplemented(_)), "got {err:?}");
    }
}
