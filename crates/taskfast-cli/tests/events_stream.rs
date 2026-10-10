// SPDX-License-Identifier: MIT
//! Contract test for `taskfast events stream --ack-stdin` (gh#169).
//!
//! Runs the real binary against a fake Phoenix channel that mirrors the
//! server's `AgentChannel`: one event in flight, the next one pushed only
//! after the in-flight one is acked over the socket. If the stream stops
//! acking on its own WebSocket, event B never arrives and the test times out.

use std::process::Stdio;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::TcpListener;
use tokio::process::{ChildStdout, Command};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

const EVENT_A: &str = "00000000-0000-0000-0000-0000000000a1";
const EVENT_B: &str = "00000000-0000-0000-0000-0000000000b2";

fn text(frame: Value) -> Message {
    Message::Text(frame.to_string().into())
}

fn push(event_id: &str) -> Message {
    text(json!(["1", null, "agent:me", "event", {
        "event_id": event_id,
        "event": "task_assigned",
        "task_id": null,
        "timestamp": "2026-10-10T00:00:00Z",
        "data": {},
    }]))
}

/// Fake `AgentChannel`: join → push A → only on `ack` for A, reply and push B.
// The handshake callback's large `Err` type is fixed by tungstenite's `Callback` trait.
#[allow(clippy::result_large_err)]
async fn fake_channel(listener: TcpListener) {
    let (tcp, _) = listener.accept().await.unwrap();
    let mut ws = tokio_tungstenite::accept_hdr_async(tcp, |_: &Request, mut resp: Response| {
        // The CLI treats a handshake without this subprotocol as an auth failure.
        resp.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("taskfast.v1"),
        );
        Ok(resp)
    })
    .await
    .unwrap();

    while let Some(Ok(msg)) = ws.next().await {
        let Message::Text(raw) = msg else { continue };
        let frame: Value = serde_json::from_str(&raw).unwrap();
        let (join_ref, msg_ref, payload) = (&frame[0], &frame[1], &frame[4]);
        match frame[3].as_str().unwrap_or("") {
            "phx_join" => {
                let reply = json!([join_ref, msg_ref, "agent:me", "phx_reply",
                    { "status": "ok", "response": {} }]);
                ws.send(text(reply)).await.unwrap();
                ws.send(push(EVENT_A)).await.unwrap();
            }
            "ack" if payload["event_id"] == EVENT_A => {
                let reply = json!([join_ref, msg_ref, "agent:me", "phx_reply",
                    { "status": "ok", "response": { "event_id": EVENT_A } }]);
                ws.send(text(reply)).await.unwrap();
                ws.send(push(EVENT_B)).await.unwrap();
            }
            _ => {}
        }
    }
}

/// Next stdout JSONL line. The server's ack timeout is 300 s, so 10 s
/// cannot be satisfied by a redelivery — only by a real ack.
async fn next_json(lines: &mut Lines<BufReader<ChildStdout>>) -> Value {
    let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .expect("timed out waiting for stream output")
        .expect("read stdout")
        .expect("stream exited early");
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON line {line:?}: {e}"))
}

#[tokio::test]
async fn ack_stdin_acks_over_socket_and_unblocks_next_event() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(fake_channel(listener));
    let cfg = tempfile::tempdir().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_taskfast"))
        .args(["--env", "local", "--allow-custom-endpoints", "--no-trace"])
        .args(["--api-base", &format!("http://127.0.0.1:{port}")])
        .args(["--api-key", "test-key"])
        .args(["events", "stream", "--ack-stdin", "--no-reconnect"])
        .env("TASKFAST_CONFIG", cfg.path().join("config.json"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();

    assert_eq!(
        next_json(&mut lines).await,
        json!({ "sentinel": "connected" })
    );
    assert_eq!(next_json(&mut lines).await["event_id"], EVENT_A);

    stdin
        .write_all(format!("{EVENT_A}\n").as_bytes())
        .await
        .unwrap();

    assert_eq!(
        next_json(&mut lines).await,
        json!({ "sentinel": "acked", "event_id": EVENT_A })
    );
    assert_eq!(next_json(&mut lines).await["event_id"], EVENT_B);

    // SIGTERM must exit promptly even though the host still holds stdin
    // open and an ack read is pending.
    let pid = child.id().unwrap().to_string();
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(killed.success());
    assert_eq!(
        next_json(&mut lines).await,
        json!({ "sentinel": "closing" })
    );
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("stream hung on exit with stdin open")
        .unwrap();
    assert!(status.success(), "{status:?}");
    drop(stdin);
}
