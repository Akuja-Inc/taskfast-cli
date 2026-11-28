// SPDX-License-Identifier: MIT
//! Wiremock fixtures for single-page event reads.

use taskfast_agent::events::{list_events_page, list_events_page_tolerant};
use taskfast_client::TaskFastClient;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> TaskFastClient {
    TaskFastClient::from_api_key(&server.uri(), "test-key").expect("build client")
}

fn event(id: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "event_id": id,
        "event": name,
        "data": { "note": "x" },
        "occurred_at": "2026-03-23T12:00:00Z",
    })
}

fn page(
    events: Vec<serde_json::Value>,
    next_cursor: Option<&str>,
    has_more: bool,
) -> serde_json::Value {
    serde_json::json!({
        "data": events,
        "meta": {
            "has_more": has_more,
            "next_cursor": next_cursor,
            "total_count": 0,
        },
    })
}

#[tokio::test]
async fn list_events_page_passes_cursor_and_limit_as_query_params() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me/events"))
        .and(query_param("cursor", "opaque-cursor-abc"))
        .and(query_param("limit", "25"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![event(
                "11111111-1111-1111-1111-111111111111",
                "task_assigned",
            )],
            None,
            false,
        )))
        .mount(&server)
        .await;

    let resp = list_events_page(&client(&server), Some("opaque-cursor-abc"), Some(25))
        .await
        .expect("happy path");
    assert_eq!(resp.data.len(), 1);
    assert_eq!(resp.data[0].event, "task_assigned");
}

#[tokio::test]
async fn tolerant_page_surfaces_good_events_alongside_unparseable() {
    let server = MockServer::start().await;
    // Mix: one well-formed event, one missing the required `event_id`.
    // Strict decode would fail the whole page (exit 6); tolerant decode
    // yields 1 good + 1 unparseable entry.
    let good = serde_json::json!({
        "event_id": "11111111-1111-1111-1111-111111111111",
        "event": "task_assigned",
        "data": {},
        "occurred_at": "2026-04-20T00:00:00Z",
    });
    let bad = serde_json::json!({
        "event": "task_assigned",
        "data": {},
        "occurred_at": "2026-04-20T00:00:00Z",
    });
    Mock::given(method("GET"))
        .and(path("/agents/me/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![good, bad], None, false)))
        .mount(&server)
        .await;

    let out = list_events_page_tolerant(&client(&server), None, Some(25))
        .await
        .expect("tolerant page decode");
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].event, "task_assigned");
    assert_eq!(out.unparseable.len(), 1);
    assert!(out.unparseable[0].error.contains("event_id"));
}

#[tokio::test]
async fn tolerant_page_bails_when_meta_is_broken() {
    let server = MockServer::start().await;
    // Meta missing has_more -> envelope broken -> bail. We cannot advance
    // the cursor on a malformed envelope, so per-item tolerance doesn't apply.
    Mock::given(method("GET"))
        .and(path("/agents/me/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [],
            "meta": { "next_cursor": null },
        })))
        .mount(&server)
        .await;

    let err = list_events_page_tolerant(&client(&server), None, Some(25))
        .await
        .expect_err("meta malformed must fail");
    assert!(
        matches!(err, taskfast_client::Error::Decode(_)),
        "expected Decode, got {err:?}"
    );
}
