// SPDX-License-Identifier: MIT
//! `TaskFastClient::call` unwraps a generated call and maps its error exactly
//! like the `into_inner()` + `map_api_error` idiom it replaces.

use taskfast_client::{map_api_error, Error, TaskFastClient};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn serve(response: ResponseTemplate) -> (MockServer, TaskFastClient) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/platform/config"))
        .respond_with(response)
        .mount(&server)
        .await;
    let client = TaskFastClient::from_api_key(&server.uri(), "k").unwrap();
    (server, client)
}

fn envelope(status: u16, code: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .set_body_json(serde_json::json!({ "error": code, "message": message }))
}

#[tokio::test]
async fn call_returns_the_unwrapped_body_on_200() {
    let (_server, client) =
        serve(ResponseTemplate::new(200).set_body_json(serde_json::json!({}))).await;
    let old = client
        .inner()
        .get_platform_config()
        .await
        .unwrap()
        .into_inner();
    let new = client.call(|c| c.get_platform_config()).await.unwrap();
    assert_eq!(
        serde_json::to_value(&old).unwrap(),
        serde_json::to_value(&new).unwrap()
    );
}

#[tokio::test]
async fn call_maps_every_error_class_like_map_api_error() {
    for (status, template) in [
        (401, envelope(401, "invalid_api_key", "nope")),
        (409, envelope(409, "conflict_code", "state clash")),
        (422, envelope(422, "missing_field", "name is required")),
        (503, ResponseTemplate::new(503)),
    ] {
        let (_server, client) = serve(template).await;
        let old = map_api_error(client.inner().get_platform_config().await.unwrap_err()).await;
        let new = client.call(|c| c.get_platform_config()).await.unwrap_err();
        assert_eq!(format!("{old:?}"), format!("{new:?}"), "status {status}");
        match (status, &new) {
            (401, Error::Auth(_)) | (503, Error::Server(_)) => {}
            (409 | 422, Error::Validation { code, .. }) => assert_ne!(code, ""),
            other => panic!("unexpected mapping {other:?}"),
        }
    }
}
