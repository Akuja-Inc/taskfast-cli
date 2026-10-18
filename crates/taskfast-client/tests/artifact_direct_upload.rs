// SPDX-License-Identifier: MIT
//! Wiremock tests for the direct-to-R2 artifact upload primitives (gh#193):
//! initiate, the presigned PUT, and complete.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use taskfast_client::{put_presigned_upload, ArtifactUpload, Error, RetryPolicy, TaskFastClient};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TASK: &str = "11111111-1111-1111-1111-111111111111";
const UPLOAD: &str = "33333333-3333-3333-3333-333333333333";

fn task_id() -> uuid::Uuid {
    TASK.parse().unwrap()
}

fn client(server: &MockServer) -> TaskFastClient {
    client_at(&server.uri())
}

fn client_at(base_url: &str) -> TaskFastClient {
    TaskFastClient::from_api_key(base_url, "test-key-123")
        .unwrap()
        .with_retry_policy(RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
        })
}

/// Write `bytes` to a fresh temp file; returns its path (for cleanup) and an
/// open handle for `put_presigned_upload`.
fn temp_file(bytes: &[u8]) -> (std::path::PathBuf, std::fs::File) {
    let path = std::env::temp_dir().join(format!("tf-put-{}", uuid::Uuid::new_v4()));
    std::fs::write(&path, bytes).unwrap();
    let file = std::fs::File::open(&path).unwrap();
    (path, file)
}

/// Read a raw HTTP request's head (through the blank line) off `stream`.
async fn read_request_head(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(stream.read_u8().await.unwrap());
    }
    head
}

fn artifact_json() -> serde_json::Value {
    json!({
        "id": "22222222-2222-2222-2222-222222222222",
        "filename": "clip.mp4",
        "content_type": "video/mp4",
        "size_bytes": 5,
        "created_at": "2026-01-01T00:00:00Z",
        "url": "artifacts/clip.mp4",
    })
}

fn far_future() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() + chrono::Duration::minutes(15)
}

#[tokio::test]
async fn initiate_sends_size_and_parses_put_ticket() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK}/artifacts/uploads")))
        .and(header("x-api-key", "test-key-123"))
        .and(body_json(json!({
            "filename": "clip.mp4",
            "content_type": "video/mp4",
            "size_bytes": 5,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "upload_id": UPLOAD,
            "method": "put",
            "url": "https://acct.r2.cloudflarestorage.com/pending/x?sig=1",
            "headers": {"content-type": "video/mp4"},
            "expires_at": "2026-10-17T12:15:00Z",
        })))
        .mount(&server)
        .await;

    let ticket = client(&server)
        .initiate_artifact_upload(&task_id(), "clip.mp4", "video/mp4", 5)
        .await
        .unwrap();
    match ticket {
        Some(ArtifactUpload::Put {
            upload_id,
            url,
            headers,
            ..
        }) => {
            assert_eq!(upload_id.to_string(), UPLOAD);
            assert!(url.starts_with("https://acct.r2.cloudflarestorage.com/"));
            assert_eq!(headers["content-type"], "video/mp4");
        }
        other => panic!("expected put ticket, got {other:?}"),
    }
}

#[tokio::test]
async fn initiate_parses_multipart_answer_and_404_as_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK}/artifacts/uploads")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"method": "multipart", "upload_id": null})),
        )
        .mount(&server)
        .await;
    let ticket = client(&server)
        .initiate_artifact_upload(&task_id(), "a.txt", "text/plain", 5)
        .await
        .unwrap();
    assert!(
        matches!(ticket, Some(ArtifactUpload::Multipart)),
        "{ticket:?}"
    );

    let old_server = MockServer::start().await;
    let ticket = client(&old_server)
        .initiate_artifact_upload(&task_id(), "a.txt", "text/plain", 5)
        .await
        .unwrap();
    assert!(ticket.is_none(), "404 means no direct upload endpoint");
}

#[tokio::test]
async fn initiate_rejects_non_uuid_upload_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK}/artifacts/uploads")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "upload_id": "../../agents/me",
            "method": "put",
            "url": "https://acct.r2.cloudflarestorage.com/x",
            "headers": {},
            "expires_at": "2026-10-17T12:15:00Z",
        })))
        .mount(&server)
        .await;
    let err = client(&server)
        .initiate_artifact_upload(&task_id(), "a.txt", "text/plain", 5)
        .await
        .expect_err("path-unsafe upload_id must not decode");
    assert!(matches!(err, Error::Decode(_)), "{err:?}");
}

#[tokio::test]
async fn initiate_413_passes_on_server_message_and_max_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/tasks/{TASK}/artifacts/uploads")))
        .respond_with(ResponseTemplate::new(413).set_body_json(json!({
            "error": "file_too_large",
            "message": "file size 300000000 bytes exceeds maximum of 200 MB",
            "max_bytes": 209_715_200,
        })))
        .mount(&server)
        .await;
    let err = client(&server)
        .initiate_artifact_upload(&task_id(), "clip.mp4", "video/mp4", 300_000_000)
        .await
        .expect_err("413");
    match err {
        Error::Validation { code, message } => {
            assert_eq!(code, "file_too_large");
            assert!(message.starts_with("file size 300000000 bytes exceeds maximum of 200 MB"));
            assert!(message.contains("209715200"), "{message}");
        }
        other => panic!("expected validation, got {other:?}"),
    }
}

#[tokio::test]
async fn put_streams_file_with_exact_headers_and_no_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/pending/x"))
        .and(header("content-type", "video/mp4"))
        .and(header("content-length", "5"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let (path, file) = temp_file(b"hello");
    let headers = HashMap::from([("content-type".to_string(), "video/mp4".to_string())]);
    put_presigned_upload(&format!("{}/pending/x", server.uri()), &headers, file, 5)
        .await
        .unwrap();
    std::fs::remove_file(&path).ok();

    let reqs = server.received_requests().await.unwrap();
    let put = &reqs[0];
    assert!(
        put.headers.get("x-api-key").is_none(),
        "API key leaked to storage host"
    );
    assert!(
        put.headers.get("transfer-encoding").is_none(),
        "chunked PUT"
    );
    assert_eq!(put.body, b"hello");
}

/// The server signs `content-length` alongside `content-type`. Sending it
/// twice gets HTTP 400 from R2's edge, so exactly one may reach the wire.
#[tokio::test]
async fn put_sends_one_content_length_when_server_signs_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/pending/x", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let head = String::from_utf8(read_request_head(&mut stream).await).unwrap();
        let mut body = [0; 5];
        stream.read_exact(&mut body).await.unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
            .await
            .unwrap();
        head
    });
    let (path, file) = temp_file(b"hello");
    let headers = HashMap::from([
        ("content-length".to_string(), "5".to_string()),
        ("content-type".to_string(), "video/mp4".to_string()),
    ]);
    put_presigned_upload(&url, &headers, file, 5).await.unwrap();
    std::fs::remove_file(&path).ok();

    let head = server.await.unwrap();
    let lengths: Vec<&str> = head
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .collect();
    assert_eq!(lengths, ["content-length: 5"], "{head}");
}

#[tokio::test]
async fn put_failure_is_server_error() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string("<Error>SignatureDoesNotMatch</Error>"),
        )
        .mount(&server)
        .await;
    let (path, file) = temp_file(b"hi");
    let err = put_presigned_upload(&format!("{}/p", server.uri()), &HashMap::new(), file, 2)
        .await
        .expect_err("403");
    std::fs::remove_file(&path).ok();
    match err {
        Error::Server(m) => assert!(
            m.contains("403") && m.contains("SignatureDoesNotMatch"),
            "{m}"
        ),
        other => panic!("expected server, got {other:?}"),
    }
}

#[tokio::test]
async fn put_transport_error_does_not_expose_presigned_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/pending/x?X-Amz-Credential=private-credential&X-Amz-Signature=secret-signature",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request_head(&mut stream).await;
        let mut body = [0; 5];
        stream.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"hello");
        // A malformed status line makes send() return a transport error.
        stream.write_all(b"not-http\r\n\r\n").await.unwrap();
        stream.shutdown().await.unwrap();
    });
    let (path, file) = temp_file(b"hello");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        put_presigned_upload(&url, &HashMap::new(), file, 5),
    )
    .await;
    std::fs::remove_file(&path).unwrap();
    let err = result
        .expect("PUT should fail promptly")
        .expect_err("bad HTTP response");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("storage peer should finish")
        .unwrap();
    assert!(matches!(err, Error::Network(_)), "{err:?}");
    for rendered in [err.to_string(), format!("{err:?}")] {
        assert!(!rendered.contains("private-credential"), "{rendered}");
        assert!(!rendered.contains("secret-signature"), "{rendered}");
    }
    if let Error::Network(err) = err {
        assert!(err.url().is_none(), "signed URL must not be retained");
    }
}

#[tokio::test]
async fn complete_retries_503_then_returns_artifact() {
    let server = MockServer::start().await;
    let complete = format!("/tasks/{TASK}/artifacts/uploads/{UPLOAD}/complete");
    Mock::given(method("POST"))
        .and(path(&complete))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": "ingest_unavailable", "message": "worker unreachable"
        })))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(&complete))
        .and(header("x-api-key", "test-key-123"))
        .respond_with(ResponseTemplate::new(201).set_body_json(artifact_json()))
        .mount(&server)
        .await;

    let artifact = client(&server)
        .complete_artifact_upload(&task_id(), &UPLOAD.parse().unwrap(), far_future())
        .await
        .unwrap();
    assert_eq!(artifact.filename.as_deref(), Some("clip.mp4"));
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn complete_retries_truncated_success_body_then_returns_artifact() {
    // Wiremock always sends complete bodies; a raw peer exercises a disconnect
    // after success headers, when reqwest's send() has already resolved.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let body = artifact_json().to_string();
        let responses = [
            "HTTP/1.1 201 Created\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{\"id\":"
                .to_string(),
            format!(
                "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        ];
        for response in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request_head(&mut stream).await;
            assert!(String::from_utf8(request).unwrap().starts_with(&format!(
                "POST /tasks/{TASK}/artifacts/uploads/{UPLOAD}/complete HTTP/1.1\r\n"
            )));
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    let client = client_at(&url);
    let artifact = tokio::time::timeout(
        Duration::from_secs(5),
        client.complete_artifact_upload(&task_id(), &UPLOAD.parse().unwrap(), far_future()),
    )
    .await
    .expect("complete should recover promptly")
    .expect("a body transport failure should retry complete");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("completion peer should finish")
        .unwrap();
    assert_eq!(artifact.filename.as_deref(), Some("clip.mp4"));
}

#[tokio::test]
async fn complete_stops_retrying_at_expiry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let err = client(&server)
        .complete_artifact_upload(&task_id(), &UPLOAD.parse().unwrap(), chrono::Utc::now())
        .await
        .expect_err("expired");
    assert!(matches!(err, Error::Server(_)), "{err:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn complete_does_not_retry_invalid_success_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201).set_body_string("not json"))
        .expect(1)
        .mount(&server)
        .await;
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        client(&server).complete_artifact_upload(
            &task_id(),
            &UPLOAD.parse().unwrap(),
            far_future(),
        ),
    )
    .await
    .expect("invalid JSON should be terminal")
    .expect_err("invalid JSON");
    assert!(matches!(err, Error::Decode(_)), "{err:?}");
}

#[tokio::test]
async fn complete_does_not_retry_409() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "upload_not_found", "message": "no object"
        })))
        .mount(&server)
        .await;
    let err = client(&server)
        .complete_artifact_upload(&task_id(), &UPLOAD.parse().unwrap(), far_future())
        .await
        .expect_err("409");
    assert!(matches!(err, Error::Validation { ref code, .. } if code == "upload_not_found"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
