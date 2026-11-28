// SPDX-License-Identifier: MIT
//! `taskfast events stream` — long-running WebSocket tail on `/socket/agent`.
//!
//! # Contract
//!
//! Stdout is JSONL. Each line is either an event payload (matching the
//! AsyncAPI `components.messages` shape) or a sentinel frame:
//!
//! * `{"sentinel":"connected"}`    — initial join succeeded.
//! * `{"sentinel":"reconnecting"}` — transport dropped; backoff started.
//! * `{"sentinel":"resumed"}`      — reconnect succeeded; backlog drains.
//! * `{"sentinel":"closing"}`      — SIGINT/SIGTERM received; exiting 0.
//! * `{"sentinel":"auth_failed"}`  — server rejected the API key; exits 2.
//! * `{"sentinel":"acked","event_id":"…"}` — server confirmed an ack sent
//!   via `--ack-stdin`.
//!
//! # Auth
//!
//! The API key is sent via the `Sec-WebSocket-Protocol` handshake header
//! (`taskfast.v1, bearer.<key>`). Keeping credentials off the URL line
//! keeps them out of proxy access logs and browser history-like caches.
//!
//! # Ack mechanic
//!
//! The server keeps one event in flight and pushes the next only after the
//! current one is acked as an `ack` message on this channel — there is no
//! HTTP ack route, and a second socket would kick this one (one socket per
//! agent). With `--ack-stdin` the stream reads event ids from stdin, one
//! per line, and sends each as `[join_ref, ref, "agent:me", "ack",
//! {"event_id": id}]`. Write the id after handling the event to keep
//! at-least-once delivery.
//!
//! The server only replies to an ack for the in-flight event; a stale id
//! gets no reply and no `acked` sentinel. Ids written while the socket is
//! down wait in the pipe until rejoin. An ack lost with a dying socket is
//! harmless: the server replays from its ack cursor on rejoin, so the
//! event is redelivered and can be acked again.
//!
//! # Protocol
//!
//! Phoenix Channels v2 JSON envelope: `[join_ref, msg_ref, topic, event, payload]`.
//! The server pushes marketplace events as `event == "event"`. Heartbeat
//! every 30s on topic `"phoenix"`. Reconnect uses exponential backoff
//! (1s → 2s → 4s → … → 30s cap); same backlog replays from cursor on
//! resume (server-side state).

use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::time::{sleep, Instant};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::Uri;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::tungstenite::ClientRequestBuilder;

use super::super::{CmdError, Ctx};

#[derive(Debug, Parser)]
pub struct StreamArgs {
    /// Exit after the first backlog drain (join + any pushes delivered
    /// before an idle moment). Intended for integration tests; production
    /// callers leave this off and the stream runs until SIGTERM/SIGINT.
    #[arg(long)]
    pub once: bool,

    /// Exit on first transport drop instead of reconnecting. Reverts to the
    /// pre-auto-reconnect behaviour for callers that own the respawn loop
    /// themselves (e.g. systemd unit with `Restart=on-failure`).
    #[arg(long)]
    pub no_reconnect: bool,

    /// Read event ids from stdin, one per line, and ack each on this
    /// stream's socket. The server holds every later event until the
    /// in-flight one is acked, so write the id after handling the event.
    #[arg(long)]
    pub ack_stdin: bool,
}

/// Topic the CLI joins on the socket.
const TOPIC: &str = "agent:me";
/// Ref of the `phx_join` push; its reply carries the same ref.
const JOIN_REF: &str = "1";
/// Phoenix heartbeat cadence. Matches the server default.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Reconnect backoff ceiling.
const RECONNECT_CAP: Duration = Duration::from_secs(30);

/// Entry point. Returns a process exit code because the stream sidesteps
/// the envelope wrapper: error envelopes would contaminate the JSONL
/// stream and swallow stdout buffering semantics.
pub async fn run(ctx: &Ctx, args: StreamArgs) -> ExitCode {
    let ack_stdin = args.ack_stdin;
    run_with(ctx, args, &mut std::io::stdout(), move || {
        (
            ack_stdin.then(spawn_stdin_reader),
            install_shutdown_signal(),
        )
    })
    .await
}

fn connection_target(ctx: &Ctx) -> Result<(String, String), CmdError> {
    let api_key = ctx.api_key.clone().ok_or(CmdError::MissingApiKey)?;
    let ws_url = ws_url_from_base(ctx.base_url())?;
    Ok((api_key, ws_url))
}

async fn run_with(
    ctx: &Ctx,
    args: StreamArgs,
    out: &mut (dyn Write + Send),
    live: impl FnOnce() -> (Option<UnboundedReceiver<String>>, ShutdownToken),
) -> ExitCode {
    let (api_key, ws_url) = match connection_target(ctx) {
        Ok(target) => target,
        Err(e) => {
            emit_error_envelope(out, ctx, &e);
            return e.exit_code().into();
        }
    };
    let (mut ack_lines, shutdown) = live();

    let mut backoff = Duration::from_secs(1);
    let mut first_connect = true;

    let mut intake = super::intake::EventIntake::from_ctx(ctx, None);

    loop {
        if shutdown.is_cancelled() {
            let _ = emit_sentinel(out, "closing");
            return ExitCode::SUCCESS;
        }

        match run_once(
            &mut *out,
            &ws_url,
            &api_key,
            first_connect,
            args.once,
            &shutdown,
            ack_lines.as_mut(),
            &mut intake,
        )
        .await
        {
            ConnectOutcome::AuthFailed => {
                let _ = emit_sentinel(out, "auth_failed");
                return ExitCode::from(2);
            }
            ConnectOutcome::Shutdown => {
                let _ = emit_sentinel(out, "closing");
                return ExitCode::SUCCESS;
            }
            ConnectOutcome::OnceDone => {
                return ExitCode::SUCCESS;
            }
            ConnectOutcome::OutputClosed => {
                return crate::exit::ExitCode::Server.into();
            }
            ConnectOutcome::Disconnected => {
                // Fall through to reconnect loop below.
            }
        }

        if args.no_reconnect {
            // Surface the drop as a non-zero exit so supervisors (systemd,
            // k8s) see it as a failure and respawn under their own policy.
            return ExitCode::from(1);
        }

        if emit_sentinel(out, "reconnecting").is_err() {
            return crate::exit::ExitCode::Server.into();
        }
        let wait = backoff;
        tokio::select! {
            () = sleep(wait) => {},
            () = shutdown.cancelled() => {
                let _ = emit_sentinel(out, "closing");
                return ExitCode::SUCCESS;
            }
        }
        backoff = (backoff * 2).min(RECONNECT_CAP);
        first_connect = false;
    }
}

enum ConnectOutcome {
    AuthFailed,
    Shutdown,
    OnceDone,
    OutputClosed,
    Disconnected,
}

async fn run_once(
    out: &mut (dyn Write + Send),
    ws_url: &str,
    api_key: &str,
    first_connect: bool,
    once: bool,
    shutdown: &ShutdownToken,
    mut ack_lines: Option<&mut UnboundedReceiver<String>>,
    intake: &mut super::intake::EventIntake,
) -> ConnectOutcome {
    let req = match build_request(ws_url, api_key) {
        Ok(r) => r,
        Err(_) => return ConnectOutcome::Disconnected,
    };

    let (ws, resp) = match tokio_tungstenite::connect_async(req).await {
        Ok(v) => v,
        Err(e) => {
            if is_auth_error(&e) {
                return ConnectOutcome::AuthFailed;
            }
            return ConnectOutcome::Disconnected;
        }
    };

    // Server must select `taskfast.v1`. If it didn't, either the socket
    // rejected us or a middlebox rewrote the handshake — treat as auth.
    if !has_taskfast_subprotocol(&resp) {
        return ConnectOutcome::AuthFailed;
    }

    let (mut sink, mut source) = ws.split();

    // Phoenix join
    let join_msg = phx_frame(JOIN_REF, JOIN_REF, TOPIC, "phx_join", json!({}));
    if sink.send(Message::Text(join_msg.into())).await.is_err() {
        return ConnectOutcome::Disconnected;
    }

    // Await join reply before announcing connected.
    let mut join_acked = false;
    let mut msg_ref: u64 = 1;
    let mut heartbeat_next = Instant::now() + HEARTBEAT_INTERVAL;

    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                let _ = sink.send(Message::Close(None)).await;
                return ConnectOutcome::Shutdown;
            }
            () = sleep_until(heartbeat_next) => {
                msg_ref += 1;
                let hb = phx_frame_null_join(&msg_ref.to_string(), "phoenix", "heartbeat", json!({}));
                if sink.send(Message::Text(hb.into())).await.is_err() {
                    return ConnectOutcome::Disconnected;
                }
                heartbeat_next = Instant::now() + HEARTBEAT_INTERVAL;
            }
            // Acks only make sense on a joined channel.
            line = next_ack(&mut ack_lines), if join_acked => match line {
                Some(id) => {
                    let id = id.trim();
                    if id.is_empty() {
                        continue;
                    }
                    msg_ref += 1;
                    let ack = phx_frame(JOIN_REF, &msg_ref.to_string(), TOPIC, "ack", json!({ "event_id": id }));
                    if sink.send(Message::Text(ack.into())).await.is_err() {
                        return ConnectOutcome::Disconnected;
                    }
                }
                // EOF or unreadable stdin: stop reading, keep streaming.
                None => ack_lines = None,
            },
            frame = source.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let parsed: Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(
                                    target: "taskfast::events",
                                    error = %e,
                                    "WS text frame was not valid JSON; surfacing as unparseable"
                                );
                                intake.record_unparseable();
                                if emit_unparseable(out, &text, &e.to_string()).is_err() {
                                    return ConnectOutcome::OutputClosed;
                                }
                                continue;
                            }
                        };
                        match classify_frame(&parsed) {
                            FrameKind::JoinOk => {
                                if !join_acked {
                                    join_acked = true;
                                    let sentinel = if first_connect { "connected" } else { "resumed" };
                                    if emit_sentinel(out, sentinel).is_err() {
                                        return ConnectOutcome::OutputClosed;
                                    }
                                }
                            }
                            FrameKind::JoinError => {
                                return ConnectOutcome::AuthFailed;
                            }
                            FrameKind::Event(payload) => {
                                match intake.classify_stream_payload(&payload) {
                                    super::intake::StreamItemOutcome::Duplicate { event_id } => {
                                        // Duplicate (replay after a lost ack): re-ack so the
                                        // server moves on, print nothing.
                                        msg_ref += 1;
                                        let ack = phx_frame(JOIN_REF, &msg_ref.to_string(), TOPIC, "ack", json!({ "event_id": event_id }));
                                        if sink.send(Message::Text(ack.into())).await.is_err() {
                                            return ConnectOutcome::Disconnected;
                                        }
                                    }
                                    super::intake::StreamItemOutcome::Accepted { event_id } => {
                                        if emit_event(out, &payload).is_err() {
                                            return ConnectOutcome::OutputClosed;
                                        }
                                        intake.mark_stream_event_seen(&event_id);
                                        if once {
                                            let _ = sink.send(Message::Close(None)).await;
                                            return ConnectOutcome::OnceDone;
                                        }
                                    }
                                    super::intake::StreamItemOutcome::Malformed { error } => {
                                        if emit_unparseable(out, &payload.to_string(), &error).is_err() {
                                            return ConnectOutcome::OutputClosed;
                                        }
                                        if once {
                                            let _ = sink.send(Message::Close(None)).await;
                                            return ConnectOutcome::OnceDone;
                                        }
                                    }
                                }
                            }
                            FrameKind::Acked(id) => {
                                if emit_event(out, &json!({ "sentinel": "acked", "event_id": id })).is_err() {
                                    return ConnectOutcome::OutputClosed;
                                }
                            }
                            FrameKind::Ignored => {}
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = sink.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        return ConnectOutcome::Disconnected;
                    }
                    Some(Err(_)) => {
                        return ConnectOutcome::Disconnected;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Read stdin on a plain thread. tokio's stdin read can't be cancelled, so
/// a pending read would hold the runtime open at exit for as long as the
/// host keeps the pipe open; a detached thread doesn't.
fn spawn_stdin_reader() -> UnboundedReceiver<String> {
    let (tx, rx) = unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Next ack id from stdin; never resolves once stdin is closed (`None`).
async fn next_ack(rx: &mut Option<&mut UnboundedReceiver<String>>) -> Option<String> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline <= now {
        return;
    }
    sleep(deadline - now).await;
}

enum FrameKind {
    JoinOk,
    JoinError,
    Acked(String),
    Event(Value),
    Ignored,
}

fn classify_frame(frame: &Value) -> FrameKind {
    let arr = match frame.as_array() {
        Some(a) if a.len() == 5 => a,
        _ => return FrameKind::Ignored,
    };
    let topic = arr[2].as_str().unwrap_or("");
    let event = arr[3].as_str().unwrap_or("");
    let payload = &arr[4];

    if event == "phx_reply" && topic == TOPIC {
        // Any ref other than the join's answers an ack we sent. Its error
        // replies must not reach the JoinError → auth_failed path.
        if arr[1].as_str() != Some(JOIN_REF) {
            return payload
                .pointer("/response/event_id")
                .and_then(Value::as_str)
                .map_or(FrameKind::Ignored, |id| FrameKind::Acked(id.to_string()));
        }
        let status = payload.get("status").and_then(|v| v.as_str()).unwrap_or("");
        return match status {
            "ok" => FrameKind::JoinOk,
            "error" => FrameKind::JoinError,
            _ => FrameKind::Ignored,
        };
    }
    if event == "event" && topic == TOPIC {
        return FrameKind::Event(payload.clone());
    }
    FrameKind::Ignored
}

fn phx_frame(join_ref: &str, msg_ref: &str, topic: &str, event: &str, payload: Value) -> String {
    serde_json::to_string(&json!([join_ref, msg_ref, topic, event, payload]))
        .expect("phoenix frame must serialize")
}

fn phx_frame_null_join(msg_ref: &str, topic: &str, event: &str, payload: Value) -> String {
    serde_json::to_string(&json!([Value::Null, msg_ref, topic, event, payload]))
        .expect("phoenix frame must serialize")
}

fn build_request(
    ws_url: &str,
    api_key: &str,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, CmdError> {
    let uri: Uri =
        ws_url
            .parse()
            .map_err(|e: tokio_tungstenite::tungstenite::http::uri::InvalidUri| {
                CmdError::Usage(format!("invalid ws url {ws_url:?}: {e}"))
            })?;
    let builder = ClientRequestBuilder::new(uri)
        .with_sub_protocol("taskfast.v1")
        .with_sub_protocol(format!("bearer.{api_key}"));
    builder
        .into_client_request()
        .map_err(|e| CmdError::Usage(format!("ws request build failed: {e}")))
}

fn has_taskfast_subprotocol(
    resp: &tokio_tungstenite::tungstenite::handshake::client::Response,
) -> bool {
    resp.headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.split(',').any(|tok| tok.trim() == "taskfast.v1"))
}

fn is_auth_error(e: &tokio_tungstenite::tungstenite::Error) -> bool {
    use tokio_tungstenite::tungstenite::Error as TE;
    match e {
        TE::Http(resp) => matches!(resp.status().as_u16(), 401 | 403),
        _ => false,
    }
}

fn ws_url_from_base(base: &str) -> Result<String, CmdError> {
    let trimmed = base.trim_end_matches('/');
    let swapped = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
        trimmed.to_string()
    } else {
        return Err(CmdError::Usage(format!(
            "base url must start with http(s):// or ws(s)://, got {base:?}"
        )));
    };
    Ok(format!("{swapped}/socket/agent/websocket?vsn=2.0.0"))
}

fn emit_sentinel(out: &mut dyn Write, kind: &str) -> std::io::Result<()> {
    emit_event(out, &json!({ "sentinel": kind }))
}

fn emit_event(out: &mut dyn Write, payload: &impl serde::Serialize) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(payload).map_err(std::io::Error::from)?;
    buf.push(b'\n');
    out.write_all(&buf)?;
    out.flush()
}

/// Surface a malformed WS text frame on the `--json` path instead of
/// swallowing it. The `type: "unparseable"` tag keeps consumers' stream
/// parsers from mistaking it for a regular event.
fn emit_unparseable(out: &mut dyn Write, raw: &str, error: &str) -> std::io::Result<()> {
    let payload = json!({ "type": "unparseable", "raw": raw, "error": error });
    emit_event(out, &payload)
}

fn emit_error_envelope(out: &mut dyn Write, ctx: &Ctx, err: &CmdError) {
    if ctx.quiet {
        return;
    }
    let env = crate::envelope::Envelope::error(ctx.environment, ctx.dry_run, err);
    let _ = emit_event(out, &env);
}

#[derive(Clone)]
struct ShutdownToken {
    inner: Arc<tokio::sync::Notify>,
    flag: Arc<std::sync::atomic::AtomicBool>,
}

impl ShutdownToken {
    fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::Notify::new()),
            flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn trigger(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn cancelled(&self) {
        let notified = self.inner.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

fn install_shutdown_signal() -> ShutdownToken {
    let token = ShutdownToken::new();
    let tclone = token.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => {
                    let _ = tokio::signal::ctrl_c().await;
                    tclone.trigger();
                    return;
                }
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        tclone.trigger();
    });
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_swaps_http_to_ws() {
        assert_eq!(
            ws_url_from_base("http://localhost:4000").unwrap(),
            "ws://localhost:4000/socket/agent/websocket?vsn=2.0.0"
        );
    }

    #[test]
    fn ws_url_swaps_https_to_wss() {
        assert_eq!(
            ws_url_from_base("https://api.taskfast.app/").unwrap(),
            "wss://api.taskfast.app/socket/agent/websocket?vsn=2.0.0"
        );
    }

    #[test]
    fn ws_url_passthrough_ws_scheme() {
        assert_eq!(
            ws_url_from_base("ws://localhost:4000").unwrap(),
            "ws://localhost:4000/socket/agent/websocket?vsn=2.0.0"
        );
    }

    #[test]
    fn ws_url_rejects_unknown_scheme() {
        assert!(ws_url_from_base("ftp://x").is_err());
    }

    #[test]
    fn classify_recognizes_join_ok() {
        let f = json!(["1", "1", "agent:me", "phx_reply", { "status": "ok", "response": {} }]);
        assert!(matches!(classify_frame(&f), FrameKind::JoinOk));
    }

    #[test]
    fn classify_recognizes_join_error() {
        let f = json!(["1", "1", "agent:me", "phx_reply", { "status": "error" }]);
        assert!(matches!(classify_frame(&f), FrameKind::JoinError));
    }

    #[test]
    fn classify_recognizes_event_push() {
        let f =
            json!(["1", null, "agent:me", "event", { "event_id": "x", "event": "task_assigned" }]);
        match classify_frame(&f) {
            FrameKind::Event(p) => assert_eq!(p["event"], "task_assigned"),
            _ => panic!("expected Event"),
        }
    }

    #[test]
    fn classify_recognizes_ack_reply() {
        let f = json!(["1", "2", "agent:me", "phx_reply", { "status": "ok", "response": { "event_id": "x" } }]);
        match classify_frame(&f) {
            FrameKind::Acked(id) => assert_eq!(id, "x"),
            _ => panic!("expected Acked"),
        }
    }

    #[test]
    fn classify_ignores_non_join_error_reply() {
        let f = json!(["1", "2", "agent:me", "phx_reply", { "status": "error", "response": { "reason": "event_id required" } }]);
        assert!(matches!(classify_frame(&f), FrameKind::Ignored));
    }

    #[test]
    fn classify_ignores_other_topics() {
        let f = json!([null, "2", "phoenix", "phx_reply", { "status": "ok" }]);
        assert!(matches!(classify_frame(&f), FrameKind::Ignored));
    }

    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use tokio_tungstenite::tungstenite::http::HeaderValue;

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

    #[allow(clippy::result_large_err)]
    async fn fake_channel(listener: TcpListener, initial: Vec<Message>) {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_hdr_async(tcp, |_: &Request, mut resp: Response| {
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
                    for msg in initial.clone() {
                        ws.send(msg).await.unwrap();
                    }
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

    fn ctx_for(port: u16, cfg: &std::path::Path) -> Ctx {
        Ctx {
            api_key: Some("test-key".into()),
            api_base: Some(format!("http://127.0.0.1:{port}")),
            config_path: cfg.join("config.json"),
            allow_custom_endpoints: true,
            ..Default::default()
        }
    }

    fn args(once: bool) -> StreamArgs {
        StreamArgs {
            once,
            no_reconnect: true,
            ack_stdin: false,
        }
    }

    fn lines(buf: &[u8]) -> Vec<Value> {
        String::from_utf8(buf.to_vec())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    struct ConnectedWriteFails;

    impl Write for ConnectedWriteFails {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if String::from_utf8_lossy(data).contains("connected") {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn replayed_event_is_printed_once_and_still_acked() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(fake_channel(listener, vec![push(EVENT_A), push(EVENT_A)]));
        let cfg = tempfile::tempdir().unwrap();
        let ctx = ctx_for(port, cfg.path());
        let shutdown = ShutdownToken::new();
        let mut writer = StopOnB {
            buf: Vec::new(),
            shutdown: shutdown.clone(),
        };

        let code = tokio::time::timeout(
            Duration::from_secs(10),
            run_with(&ctx, args(false), &mut writer, || (None, shutdown)),
        )
        .await
        .expect("stream did not finish");

        assert_eq!(code, ExitCode::SUCCESS);
        let lines = lines(&writer.buf);
        assert_eq!(lines[0], json!({ "sentinel": "connected" }));
        assert_eq!(lines[1]["event_id"], EVENT_A);
        assert_eq!(
            lines[2],
            json!({ "sentinel": "acked", "event_id": EVENT_A })
        );
        assert_eq!(lines[3]["event_id"], EVENT_B);
        assert_eq!(
            lines
                .iter()
                .filter(|l| l["event_id"] == EVENT_A && l.get("sentinel").is_none())
                .count(),
            1
        );
        assert!(cfg.path().join("events.seen").exists());
    }

    #[tokio::test]
    async fn stream_once_terminates_on_malformed_event_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(fake_channel(
            listener,
            vec![text(json!(["1", null, "agent:me", "event", {
                "event": "corrupted_event",
                "data": {}
            }]))],
        ));
        let cfg = tempfile::tempdir().unwrap();
        let ctx = ctx_for(port, cfg.path());
        let mut out: Vec<u8> = Vec::new();

        let code = tokio::time::timeout(
            Duration::from_secs(5),
            run_with(&ctx, args(true), &mut out, || (None, ShutdownToken::new())),
        )
        .await
        .expect("stream timed out waiting to exit under --once");

        assert_eq!(code, ExitCode::SUCCESS);
        let lines = lines(&out);
        assert_eq!(lines[0], json!({ "sentinel": "connected" }));
        assert_eq!(lines[1]["type"], "unparseable");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(fake_channel(
            listener,
            vec![text(json!(["1", null, "agent:me", "event", {
                "event": "corrupted_event",
                "data": {}
            }]))],
        ));
        let ctx = ctx_for(port, cfg.path());
        let mut failing = UnparseableWriteFails;
        let code = tokio::time::timeout(
            Duration::from_secs(5),
            run_with(&ctx, args(true), &mut failing, || {
                (None, ShutdownToken::new())
            }),
        )
        .await
        .expect("stream timed out waiting to exit under --once");
        assert_eq!(code, ExitCode::from(6));
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn failed_write_closes_without_a_close_frame_or_ack() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws =
                tokio_tungstenite::accept_hdr_async(tcp, |_: &Request, mut resp: Response| {
                    resp.headers_mut().insert(
                        "sec-websocket-protocol",
                        HeaderValue::from_static("taskfast.v1"),
                    );
                    Ok(resp)
                })
                .await
                .unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(raw) = &msg {
                    let frame: Value = serde_json::from_str(raw).unwrap();
                    if frame[3].as_str() == Some("phx_join") {
                        let reply = json!([&frame[0], &frame[1], "agent:me", "phx_reply",
                            { "status": "ok", "response": {} }]);
                        ws.send(text(reply)).await.unwrap();
                        ws.send(push(EVENT_A)).await.unwrap();
                    }
                }
                let _ = tx.send(msg);
            }
        });
        let cfg = tempfile::tempdir().unwrap();
        let ctx = ctx_for(port, cfg.path());
        let mut failing = EventFlushFails {
            last_was_event: false,
        };

        let code = tokio::time::timeout(
            Duration::from_secs(5),
            run_with(&ctx, args(false), &mut failing, || {
                (None, ShutdownToken::new())
            }),
        )
        .await
        .expect("stream did not finish");
        assert_eq!(code, ExitCode::from(6));

        sleep(Duration::from_millis(200)).await;
        let mut received = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            received.push(msg);
        }
        assert!(
            received
                .iter()
                .all(|m| matches!(m, Message::Text(t) if t.contains("phx_join"))),
            "{received:?}"
        );
    }

    struct UnparseableWriteFails;

    impl Write for UnparseableWriteFails {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if String::from_utf8_lossy(data).contains("unparseable") {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct StopOnB {
        buf: Vec<u8>,
        shutdown: ShutdownToken,
    }

    impl Write for StopOnB {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(data);
            if String::from_utf8_lossy(data).contains(EVENT_B) {
                self.shutdown.trigger();
            }
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct EventFlushFails {
        last_was_event: bool,
    }

    impl Write for EventFlushFails {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.last_was_event = String::from_utf8_lossy(data).contains("event_id");
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.last_was_event {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn event_is_never_persisted_when_flush_fails_and_a_fresh_run_emits_it() {
        let cfg = tempfile::tempdir().unwrap();
        let seen_path = cfg.path().join("events.seen");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(fake_channel(listener, vec![push(EVENT_A)]));
        let ctx = ctx_for(port, cfg.path());
        let mut failing = EventFlushFails {
            last_was_event: false,
        };
        let code = tokio::time::timeout(
            Duration::from_secs(5),
            run_with(&ctx, args(true), &mut failing, || {
                (None, ShutdownToken::new())
            }),
        )
        .await
        .expect("stream did not finish");
        assert_eq!(code, ExitCode::from(6));
        assert!(!seen_path.exists());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(fake_channel(listener, vec![push(EVENT_A)]));
        let ctx = ctx_for(port, cfg.path());
        let mut out: Vec<u8> = Vec::new();
        let code = tokio::time::timeout(
            Duration::from_secs(5),
            run_with(&ctx, args(true), &mut out, || (None, ShutdownToken::new())),
        )
        .await
        .expect("stream did not finish");
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(lines(&out)[1]["event_id"], EVENT_A);
        assert!(std::fs::read_to_string(&seen_path)
            .unwrap()
            .contains(EVENT_A));
    }

    #[tokio::test]
    async fn failed_connected_write_stops_the_stream_even_with_reconnect_enabled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(fake_channel(listener, Vec::new()));
        let cfg = tempfile::tempdir().unwrap();
        let ctx = ctx_for(port, cfg.path());
        let mut out = ConnectedWriteFails;

        let code = tokio::time::timeout(
            Duration::from_secs(5),
            run_with(
                &ctx,
                StreamArgs {
                    once: false,
                    no_reconnect: false,
                    ack_stdin: false,
                },
                &mut out,
                || (None, ShutdownToken::new()),
            ),
        )
        .await
        .expect("stream kept running after a failed connected write");

        assert_eq!(code, ExitCode::from(6));
    }

    #[tokio::test]
    async fn missing_api_key_envelope_goes_to_injected_writer() {
        let cfg = tempfile::tempdir().unwrap();
        let ctx = Ctx {
            api_key: None,
            config_path: cfg.path().join("config.json"),
            ..Default::default()
        };
        let mut out: Vec<u8> = Vec::new();

        let code = run_with(&ctx, args(true), &mut out, || {
            panic!("live I/O must not start before validation")
        })
        .await;

        assert_eq!(code, ExitCode::from(2));
        let raw = String::from_utf8(out.clone()).unwrap();
        assert!(raw.ends_with('\n'));
        let lines = lines(&out);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["ok"], false);
        assert_eq!(lines[0]["error"]["code"], "missing_api_key");
    }

    fn assert_send<T: Send>(_: T) {}

    #[test]
    fn run_future_is_send() {
        let ctx = Ctx::default();
        assert_send(run(&ctx, args(true)));
    }
}
