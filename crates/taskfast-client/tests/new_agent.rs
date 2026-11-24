// SPDX-License-Identifier: MIT
//! `POST /new` through `TaskFastClient` (gh#238): the call carries no
//! credential, each documented refusal lands on its typed outcome, anything
//! the server did not say stays the generic error, and the method never
//! retries — the server counts every request against the caller's daily
//! quota, refused ones included.

use std::time::Duration;

use taskfast_client::api::types::{
    NewAgentRequest, NewAgentRequestDescription, NewAgentRequestName,
};
use taskfast_client::{Error, NewAgentOutcome, TaskFastClient};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CLAIM_URL: &str = "https://staging.taskfast.app/new/claim?claim_token=tok";
const TERMS_URL: &str = "https://staging.taskfast.app/terms/agents";
const NOT_READY_MSG: &str = "The Terms of Service are not published yet; try again later.";

fn body() -> NewAgentRequest {
    NewAgentRequest {
        name: Some(NewAgentRequestName::try_from("taskfast-agent").expect("valid name")),
        description: Some(
            NewAgentRequestDescription::try_from("Headless agent registered via taskfast init")
                .expect("valid description"),
        ),
    }
}

/// The shape `NewAgentJSON.created/1` sends, field for field.
fn created_body() -> serde_json::Value {
    serde_json::json!({
        "agent_id": "11111111-1111-1111-1111-111111111111",
        "api_key": "am_live_fresh",
        "claim_url": CLAIM_URL,
        "expires_at": "2026-11-26T17:48:51Z",
        "terms_accepted": true,
        "terms_url": TERMS_URL,
        "terms_version": "2026-11-15",
        "terms_notice": format!(
            "By sending this request you accepted the TaskFast Terms of Service at {TERMS_URL}."
        ),
        "wallet_required": true,
        "quick_start": "Set a wallet with POST /api/agents/me/wallet, …",
        "links": {
            "mcp": "https://staging.taskfast.app/mcp",
            "openapi": "https://staging.taskfast.app/api/docs/spec",
            "tasks": "https://staging.taskfast.app/api/tasks",
            "wallet": "https://staging.taskfast.app/api/agents/me/wallet",
        },
    })
}

fn anonymous(server: &MockServer) -> TaskFastClient {
    TaskFastClient::anonymous(&server.uri()).expect("construct client")
}

#[tokio::test]
async fn creates_unclaimed_agent_with_no_credential() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .and(body_json(serde_json::json!({
            "name": "taskfast-agent",
            "description": "Headless agent registered via taskfast init",
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(created_body()))
        .expect(1)
        .mount(&server)
        .await;

    let created = match anonymous(&server).create_unclaimed_agent(&body()).await {
        Ok(NewAgentOutcome::Created(created)) => *created,
        other => panic!("expected Created, got {other:?}"),
    };

    assert_eq!(created.api_key.as_deref(), Some("am_live_fresh"));
    assert_eq!(
        created.agent_id.map(|id| id.to_string()).as_deref(),
        Some("11111111-1111-1111-1111-111111111111")
    );
    assert_eq!(created.claim_url.as_deref(), Some(CLAIM_URL));
    assert_eq!(
        created.expires_at.map(|t| t.to_string()),
        Some("2026-11-26T17:48:51Z".to_string())
    );
    assert_eq!(created.terms_version.as_deref(), Some("2026-11-15"));
    assert_eq!(created.terms_accepted, Some(true));
    assert_eq!(created.wallet_required, Some(true));

    // Anonymous by contract: no key header may leave the process.
    let received = server.received_requests().await.expect("recorded requests");
    assert_eq!(received.len(), 1, "POST /new is sent exactly once");
    assert!(
        received[0].headers.get("x-api-key").is_none(),
        "X-API-Key must not be sent to POST /new"
    );
}

#[tokio::test]
async fn status_404_with_server_body_is_route_off() {
    // The `RequireNewAgentEnabled` plug's answer while the switch is off.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": "not_found",
            "message": "The requested resource does not exist",
        })))
        .mount(&server)
        .await;

    let outcome = anonymous(&server).create_unclaimed_agent(&body()).await;
    assert!(
        matches!(outcome, Ok(NewAgentOutcome::RouteOff)),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn status_404_without_body_is_still_route_off() {
    // An edge or proxy in front of the app answers 404 with no JSON. The
    // caller branches on the outcome, so it must not depend on a body.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let outcome = anonymous(&server).create_unclaimed_agent(&body()).await;
    assert!(
        matches!(outcome, Ok(NewAgentOutcome::RouteOff)),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn status_429_from_the_server_is_the_daily_limit() {
    // `RateLimitNewAgent`: per-address daily limit, with both the header
    // and the body's `retry_after_seconds`. The header wins.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "86400")
                .set_body_json(serde_json::json!({
                    "error": "rate_limited",
                    "message": "Too many new agents from this address; retry after the rate-limit window expires.",
                    "retry_after_seconds": 86_400,
                })),
        )
        .mount(&server)
        .await;

    let outcome = anonymous(&server).create_unclaimed_agent(&body()).await;
    match outcome {
        Ok(NewAgentOutcome::RateLimited { retry_after }) => {
            assert_eq!(retry_after, Duration::from_hours(24));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn status_429_without_header_uses_the_body_hint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
            "error": "rate_limited",
            "message": "Too many new agents from this address.",
            "retry_after_seconds": 5,
        })))
        .mount(&server)
        .await;

    let outcome = anonymous(&server).create_unclaimed_agent(&body()).await;
    match outcome {
        Ok(NewAgentOutcome::RateLimited { retry_after }) => {
            assert_eq!(retry_after, Duration::from_secs(5));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn status_429_from_an_edge_is_not_the_daily_limit() {
    // A WAF's or proxy's 429 carries no TaskFast envelope: it must not be
    // reported as the server's per-address quota (gh#238 review). It keeps
    // the generic 429 mapping every other call gets.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "30")
                .set_body_string("<html>Too many requests</html>"),
        )
        .mount(&server)
        .await;

    let outcome = anonymous(&server).create_unclaimed_agent(&body()).await;
    match outcome {
        Err(Error::RateLimited { retry_after }) => assert_eq!(retry_after, Duration::from_secs(30)),
        other => panic!("expected generic RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn status_503_from_the_server_is_not_ready_and_is_never_retried() {
    // `{:error, :terms_unpublished}` while `agent_terms_published` is
    // false. 5xx is retryable everywhere else in the client; here a retry
    // would spend the caller's quota for nothing, so exactly one request.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": "try_later",
            "message": NOT_READY_MSG,
            "retry_after_seconds": 3600,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let outcome = anonymous(&server).create_unclaimed_agent(&body()).await;
    match outcome {
        Ok(NewAgentOutcome::NotReady { message }) => assert_eq!(message, NOT_READY_MSG),
        other => panic!("expected NotReady, got {other:?}"),
    }
}

#[tokio::test]
async fn status_503_without_envelope_is_a_generic_server_error() {
    // A load balancer's page during a deploy is not the server saying its
    // Terms are unpublished; the generic error keeps the raw body and the
    // caller does not quote it as the server's wording.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(
            ResponseTemplate::new(503).set_body_string("<html>Service Unavailable</html>"),
        )
        .mount(&server)
        .await;

    let outcome = anonymous(&server).create_unclaimed_agent(&body()).await;
    match outcome {
        Err(Error::Server(msg)) => assert_eq!(msg, "HTTP 503: <html>Service Unavailable</html>"),
        other => panic!("expected generic Server, got {other:?}"),
    }
}

#[tokio::test]
async fn status_422_keeps_the_server_validation_error() {
    // E.g. an all-whitespace name: the server validates what the spec's
    // length checks cannot see. Not a "refusal" — the caller's input is
    // wrong, so it keeps the generic mapping.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "error": "validation_error",
            "message": "Invalid agent data",
            "errors": {"name": ["must be a non-empty string of at most 80 characters"]},
        })))
        .mount(&server)
        .await;

    let outcome = anonymous(&server).create_unclaimed_agent(&body()).await;
    match outcome {
        Err(Error::Validation { code, message }) => {
            assert_eq!(code, "validation_error");
            assert_eq!(message, "Invalid agent data");
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn request_name_and_description_carry_the_spec_length_limits() {
    // The generated types are the spec's `minLength`/`maxLength`, so an
    // impossible value fails before any request is made.
    assert!("".parse::<NewAgentRequestName>().is_err());
    assert!("x".repeat(81).parse::<NewAgentRequestName>().is_err());
    assert!("x".repeat(80).parse::<NewAgentRequestName>().is_ok());
    assert!("".parse::<NewAgentRequestDescription>().is_err());
    assert!("x"
        .repeat(501)
        .parse::<NewAgentRequestDescription>()
        .is_err());
    assert!("x"
        .repeat(500)
        .parse::<NewAgentRequestDescription>()
        .is_ok());
}
