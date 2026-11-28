// SPDX-License-Identifier: MIT
//! `TaskFastClient::rpc_client` hands the keyed HTTP client only to URLs on the
//! API's own origin, so `X-API-Key` never reaches another host.

use taskfast_client::{Error, TaskFastClient};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn same_origin_url_gets_the_keyed_client() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/rpc/testnet"))
        .and(header("x-api-key", "secret-key"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let client = TaskFastClient::from_api_key(&format!("{}/", server.uri()), "secret-key").unwrap();
    let http = client
        .rpc_client(&format!("{}/rpc/testnet", server.uri()))
        .unwrap();
    let resp = http
        .post(format!("{}/rpc/testnet", server.uri()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn other_origins_are_refused() {
    let server = MockServer::start().await;
    let client = TaskFastClient::from_api_key(&server.uri(), "k").unwrap();
    let port = server.address().port();
    for url in [
        "http://127.0.0.1:1/rpc/testnet".to_string(),
        format!("http://localhost:{port}/rpc/testnet"),
        format!("https://127.0.0.1:{port}/rpc/testnet"),
        "https://rpc.example.com/".to_string(),
        "not a url".to_string(),
    ] {
        match client.rpc_client(&url) {
            Err(Error::Validation { code, .. }) => assert_eq!(code, "rpc_url_off_origin", "{url}"),
            other => panic!("{url}: expected off-origin refusal, got {other:?}"),
        }
    }
}
