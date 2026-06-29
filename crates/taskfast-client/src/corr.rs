// SPDX-License-Identifier: MIT
//! Server correlation-id (`x-request-id`) capture for the CLI trace (gh#85).
//!
//! Every TaskFast response carries an `x-request-id` header that the server's
//! `mix taskfast.trace <task_id>` joins on. The CLI needs that value to write a
//! correlated trace line, but ~50 call sites discard the response headers at
//! `ResponseValue::into_inner()`. Rather than thread the header through each
//! one, [`record_corr`] is wired as progenitor's **post-hook** (see
//! `build.rs`), so it fires for every generated request — success or error —
//! at one place. Hand-rolled reqwest calls capture via [`record_corr_response`].
//!
//! Storage is a process-global "last seen" cell. One CLI process runs exactly
//! one command, so the most recent HTTP call's id is the right `corr` for that
//! command's trace line.
// ponytail: process-global "last seen" cell — one process == one command, so
// no per-task keying needed. Revisit only if the CLI ever runs concurrent ops.

use std::sync::Mutex;

static LAST_CORR: Mutex<Option<String>> = Mutex::new(None);

const HEADER: &str = "x-request-id";

/// Progenitor post-hook: records the correlation id from a completed request.
///
/// The generated client invokes `(crate::record_corr)(&result)` after every
/// `exec`, before the response is unwrapped, so this sees both 2xx and error
/// (4xx/5xx) responses — those still join, so their id is kept.
///
/// A transport failure (`Err`) carries no response, so it *clears* the stored
/// id: otherwise a later transport error in a multi-call command would leave a
/// stale `corr` from an earlier request and mis-join the failure's trace line.
pub fn record_corr(result: &Result<reqwest::Response, reqwest::Error>) {
    match result {
        Ok(resp) => record_corr_response(resp),
        Err(_) => {
            if let Ok(mut slot) = LAST_CORR.lock() {
                *slot = None;
            }
        }
    }
}

/// Capture the correlation header from a raw response. Used by the few
/// hand-rolled reqwest calls that bypass the generated client.
pub fn record_corr_response(resp: &reqwest::Response) {
    if let Some(id) = resp.headers().get(HEADER).and_then(|h| h.to_str().ok()) {
        if let Ok(mut slot) = LAST_CORR.lock() {
            *slot = Some(id.to_string());
        }
    }
}

/// Take and clear the last captured correlation id.
pub fn take_last_corr() -> Option<String> {
    LAST_CORR.lock().ok().and_then(|mut slot| slot.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_with(id: Option<&str>) -> reqwest::Response {
        let mut builder = http::Response::builder().status(200);
        if let Some(id) = id {
            builder = builder.header(HEADER, id);
        }
        reqwest::Response::from(builder.body("{}").unwrap())
    }

    // One test, not two: `LAST_CORR` is process-global, and the test runner
    // would race two functions that both touch it.
    #[test]
    fn capture_take_missing_header_and_transport_error() {
        let _ = take_last_corr(); // start clean

        // Captures from an Ok response carrying the header.
        record_corr(&Ok(response_with(Some("req-123"))));
        assert_eq!(take_last_corr().as_deref(), Some("req-123"));

        // take() clears: a second take with no new capture sees nothing.
        assert_eq!(take_last_corr(), None);

        // A response without the header must not overwrite the prior value.
        record_corr(&Ok(response_with(Some("req-xyz"))));
        record_corr(&Ok(response_with(None)));
        assert_eq!(take_last_corr().as_deref(), Some("req-xyz"));

        // A transport failure clears the stored id so a failure trace line
        // cannot carry a stale `corr`. (error_for_status on a 5xx yields the
        // Err(reqwest::Error) we need without a live socket.)
        let err = reqwest::Response::from(http::Response::builder().status(500).body("").unwrap())
            .error_for_status();
        assert!(err.is_err());
        record_corr(&err);
        assert_eq!(take_last_corr(), None);
    }
}
