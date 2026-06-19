// SPDX-License-Identifier: MIT
//! End-to-end smoke test for the progenitor-generated client.
//!
//! Verifies that:
//!   1. The normalized spec produces a callable typed client.
//!   2. A 2xx JSON response deserializes into the expected generated type.
//!   3. A non-2xx response surfaces as `Error::UnexpectedResponse`, which is
//!      the contract our `taskfast-client::errors::Error` layer will consume.
//!
//! We deliberately pick `GET /platform/config` — no auth, no request body,
//! no path params — so the test exercises codegen wiring without fixture churn.
//!
//! Note: this test uses the **raw** `api::Client` (not `TaskFastClient`), so
//! the `/api` server prefix is NOT applied here — the wiremock path matches
//! the unprefixed spec path exactly as progenitor emits it. `TaskFastClient`-
//! based tests (e.g. `auth_and_error_mapping.rs`) register `/api/...` paths.

use taskfast_client::api::{Client, Error};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn platform_config_happy_path_roundtrips() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "submission_fee": "0.25",
        "submission_fee_currency": "USDC",
        "default_fee_tier": "open",
        "completion_fee_tiers": [
            { "tier": "open", "percent": "2.5", "rate": "0.025", "default": true, "selectable": true },
            { "tier": "high_assurance", "percent": "10", "rate": "0.10", "default": false, "selectable": true },
        ],
        "max_task_duration_days": 7,
        "default_pickup_window_hours": 24,
        "default_review_window_hours": 24,
        "default_remedy_window_hours": 48,
        "max_open_count": 3,
        "tempo_platform_wallet": "0x2237a647792d76847D7764267598DD772d97d95d",
    });
    Mock::given(method("GET"))
        .and(path("/platform/config"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = Client::new(&server.uri());
    let resp = client
        .get_platform_config()
        .await
        .expect("generated client decodes 200");

    let cfg = resp.into_inner();
    assert_eq!(cfg.submission_fee.as_deref(), Some("0.25"));
    assert_eq!(cfg.max_open_count, Some(3));

    // Two-tier completion fee (gh#52): the CLI must surface the per-tier
    // `percent` instead of a flat `completion_fee_percent`.
    assert_eq!(cfg.default_fee_tier.as_deref(), Some("open"));
    let tiers = &cfg.completion_fee_tiers;
    assert_eq!(tiers.len(), 2);
    // Look tiers up by `tier` id rather than array position — the response
    // array order is not contractually guaranteed.
    let find = |id: &str| {
        tiers
            .iter()
            .find(|t| t.tier == id)
            .unwrap_or_else(|| panic!("tier {id} present"))
    };
    let open = find("open");
    assert_eq!(open.percent, "2.5");
    assert_eq!(open.rate, "0.025");
    assert!(open.default);
    assert!(open.selectable);
    let high = find("high_assurance");
    assert!(!high.default);
}

#[tokio::test]
async fn non_2xx_surfaces_as_unexpected_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/platform/config"))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_json(serde_json::json!({ "error": "unavailable", "message": "down" })),
        )
        .mount(&server)
        .await;

    let client = Client::new(&server.uri());
    let err = client
        .get_platform_config()
        .await
        .expect_err("503 must not decode as success");

    match err {
        Error::UnexpectedResponse(resp) => {
            assert_eq!(resp.status().as_u16(), 503);
        }
        other => panic!("expected UnexpectedResponse, got {other:?}"),
    }
}
