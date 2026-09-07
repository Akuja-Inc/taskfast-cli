// SPDX-License-Identifier: MIT
//! `taskfast ping` — fast liveness probe.
//!
//! Two modes, chosen by whether an API key is present:
//!
//! * **Authenticated** (key present): `GET /agents/me` — validates
//!   network reachability, base URL, *and* key in one round-trip.
//! * **Anonymous** (no key): raw `GET` on the configured base URL —
//!   reachability only. Any HTTP response from the host counts as a pong;
//!   only a transport failure is an error. No TaskFast endpoint is
//!   unauthenticated, so the weaker signal is intentional.
//!
//! Both modes are **single-attempt** — the client's [`RetryPolicy`] is
//! bypassed on purpose. A diagnostic that silently retries hides the
//! signal the operator asked for ("is the server up *right now*?").
//!
//! Envelope `data` shape:
//! ```json
//! {
//!   "pong": true,
//!   "latency_ms": 42,
//!   "endpoint": "GET /agents/me",
//!   "base_url": "http://localhost:4000",
//!   "authenticated": true
//! }
//! ```
//!
//! [`RetryPolicy`]: taskfast_client::RetryPolicy

use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::json;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::map_api_error;
use taskfast_client::ClientInfo;

/// Connect timeout for the anonymous probe — short on purpose so `ping`
/// fails fast when the host is unreachable.
const ANON_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Total request timeout for the anonymous probe.
const ANON_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Parser)]
pub struct Args;

pub async fn run(ctx: &Ctx, _args: Args) -> CmdResult {
    let (latency_ms, endpoint, base_url, authenticated) = match ctx.api_key.as_deref() {
        Some(_) => probe_authenticated(ctx).await?,
        None => probe_anonymous(ctx).await?,
    };

    let data = json!({
        "pong": true,
        "latency_ms": latency_ms,
        "endpoint": endpoint,
        "base_url": base_url,
        "authenticated": authenticated,
    });
    Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
}

async fn probe_authenticated(ctx: &Ctx) -> Result<(u64, &'static str, String, bool), CmdError> {
    let client = ctx.client()?;
    let base_url = client.inner().baseurl().to_string();

    let started = Instant::now();
    let result = client.inner().get_agent_profile().await;
    let latency_ms = started.elapsed().as_millis() as u64;

    if let Err(e) = result {
        return Err(map_probe_error(e, &base_url).await);
    }
    Ok((latency_ms, "GET /agents/me", base_url, true))
}

/// gh#145: a 404 from a server that just answered at the HTTP layer means
/// the host is not in the server's `:api_hosts` rewrite list, so the request
/// never reached the API routes — a configuration trap, not a dead endpoint.
/// Diagnose it explicitly (the server's 404 page carries no hint, and a raw
/// HTML body must not leak into the message). Pairs with the server-side
/// `:api_hosts` issue Akuja-Inc/taskfast#1163.
async fn map_probe_error(e: taskfast_client::api::Error<()>, base_url: &str) -> CmdError {
    if let taskfast_client::api::Error::UnexpectedResponse(resp) = &e {
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return CmdError::Validation {
                code: "not_found".into(),
                message: format!(
                    "GET /agents/me returned 404 from {base_url}: the server is reachable \
                     but did not rewrite this host to its /api routes (it only rewrites \
                     hosts in its :api_hosts list). Point --api-base/TASKFAST_API at the \
                     api.<domain> host for this environment (e.g. https://api.taskfast.app), \
                     or use 127.0.0.1 against a local dev server."
                ),
            };
        }
    }
    map_api_error(e).await.into()
}

async fn probe_anonymous(ctx: &Ctx) -> Result<(u64, &'static str, String, bool), CmdError> {
    let base_url = ctx.base_url().to_string();
    let http = reqwest::Client::builder()
        .connect_timeout(ANON_CONNECT_TIMEOUT)
        .timeout(ANON_REQUEST_TIMEOUT)
        .build()
        .map_err(|e| CmdError::Network(e.to_string()))?;

    let started = Instant::now();
    let resp = http
        .get(&base_url)
        .send()
        .await
        .map_err(|e| CmdError::Network(e.to_string()))?;
    let latency_ms = started.elapsed().as_millis() as u64;

    // Any HTTP response — even 4xx/5xx — proves the host is reachable and
    // speaking HTTP. Status-code semantics need an authenticated probe.
    let _ = resp.status();

    Ok((latency_ms, "GET /", base_url, false))
}
