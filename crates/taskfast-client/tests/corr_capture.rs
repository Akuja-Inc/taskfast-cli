// SPDX-License-Identifier: MIT
//! Runtime proof that progenitor's post-hook captures the server
//! `x-request-id` for the CLI trace (gh#85), through the real generated client.
//!
//! Structural verification (the hook is present in codegen) is not enough — this
//! exercises an actual request so a future progenitor/codegen change that drops
//! the hook fails here.

use taskfast_client::{take_last_corr, TaskFastClient};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn post_hook_captures_x_request_id_through_generated_client() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/platform/config"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-request-id", "req-test-42")
                .set_body_json(serde_json::json!({})),
        )
        .mount(&server)
        .await;

    let _ = take_last_corr(); // clear any prior global state
    let client = TaskFastClient::from_api_key(&server.uri(), "k").expect("client");
    // The hook fires before the body is decoded, so the correlation id is
    // captured regardless of whether the typed decode succeeds.
    let _ = client.inner().get_platform_config().await;

    assert_eq!(take_last_corr().as_deref(), Some("req-test-42"));
}
