// SPDX-License-Identifier: MIT
//! Correlation-id capture and clearing for the hand-written client methods.
use std::collections::HashMap;

use taskfast_client::{put_presigned_upload, take_last_corr, TaskFastClient};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TASK: &str = "11111111-1111-1111-1111-111111111111";

fn with_id(status: u16, id: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .insert_header("x-request-id", id)
        .set_body_json(serde_json::json!({}))
}

fn unreachable_port() -> (String, tokio::net::TcpSocket) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let uri = format!("http://{}", socket.local_addr().unwrap());
    (uri, socket)
}

#[tokio::test]
async fn correlation_is_captured_cleared_and_never_fed_by_third_parties() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/platform/config"))
        .respond_with(with_id(200, "gen-ok"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/users/me"))
        .respond_with(with_id(404, "me-404"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(with_id(200, "raw-ok"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/config/network"))
        .respond_with(with_id(503, "raw-5xx"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK}/artifacts/uploads")))
        .respond_with(with_id(404, "init-404"))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/stored"))
        .respond_with(with_id(403, "r2-must-not-win"))
        .mount(&server)
        .await;

    let client = TaskFastClient::from_api_key(&server.uri(), "k").unwrap();
    let task: uuid::Uuid = TASK.parse().unwrap();
    let _ = take_last_corr();

    let _ = client.inner().get_platform_config().await;
    assert_eq!(take_last_corr().as_deref(), Some("gen-ok"), "generated 2xx");

    assert!(client.get_user_profile().await.unwrap().is_none());
    assert_eq!(
        take_last_corr().as_deref(),
        Some("me-404"),
        "hand-rolled 404"
    );

    client.get_agent_profile_raw().await.unwrap();
    assert_eq!(
        take_last_corr().as_deref(),
        Some("raw-ok"),
        "hand-rolled 2xx"
    );

    assert!(client.fetch_network_config().await.is_err());
    assert_eq!(
        take_last_corr().as_deref(),
        Some("raw-5xx"),
        "hand-rolled 5xx"
    );

    assert!(client
        .initiate_artifact_upload(&task, "a.txt", "text/plain", 1)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        take_last_corr().as_deref(),
        Some("init-404"),
        "initiate 404"
    );

    let _ = client.inner().get_platform_config().await;
    let file = tempfile_with(b"x");
    let put = put_presigned_upload(
        &format!("{}/stored", server.uri()),
        &HashMap::new(),
        file,
        1,
    )
    .await;
    assert!(put.is_err());
    assert_eq!(
        take_last_corr().as_deref(),
        Some("gen-ok"),
        "third-party PUT must not touch correlation"
    );

    let _ = client.inner().get_platform_config().await;
    let (dead_uri, _reserved) = unreachable_port();
    let dead = TaskFastClient::from_api_key(&dead_uri, "k").unwrap();
    assert!(dead.get_user_profile().await.is_err());
    assert_eq!(take_last_corr(), None, "hand-rolled transport error clears");

    let _ = client.inner().get_platform_config().await;
    assert!(dead.get_agent_profile_raw().await.is_err());
    assert_eq!(take_last_corr(), None, "raw transport error clears");

    let _ = client.inner().get_platform_config().await;
    assert!(dead.inner().get_platform_config().await.is_err());
    assert_eq!(take_last_corr(), None, "generated transport error clears");
}

fn tempfile_with(bytes: &[u8]) -> std::fs::File {
    let path = std::env::temp_dir().join(format!("tf-corr-{}", uuid::Uuid::new_v4()));
    std::fs::write(&path, bytes).unwrap();
    let file = std::fs::File::open(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    file
}
