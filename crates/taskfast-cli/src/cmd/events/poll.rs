// SPDX-License-Identifier: MIT
//! `taskfast events poll` — single-page read over `GET /agents/me/events`.
//!
//! The SDK also offers `stream_events` (long-running cursor chase);
//! the polling subcommand stays one-shot because a JSON-lines follow
//! mode doesn't mix with the envelope-per-invocation contract. Live
//! tailing lives in [`super::stream`] over WebSocket instead.
//!
//! # Cursor persistence
//!
//! With `--cursor` absent, the previous invocation's `next_cursor` is
//! read from `<config_dir>/events.cursor` so back-to-back polls in an
//! agent loop resume past the last batch without orchestrator
//! bookkeeping. After a successful poll the new `next_cursor` is
//! written back. `--cursor -` and `TASKFAST_NO_CURSOR_STATE=1` both
//! disable read+write for ad-hoc inspection runs that shouldn't
//! mutate the persisted offset.

use clap::Parser;
use serde_json::json;

use super::super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_agent::events::list_events_page_tolerant;

#[derive(Debug, Parser)]
pub struct PollArgs {
    /// Opaque pagination cursor from a previous response's `next_cursor`.
    /// Pass `-` to bypass the persisted cursor file (no read, no write)
    /// for one-shot inspection without mutating the agent loop's offset.
    #[arg(long)]
    pub cursor: Option<String>,

    /// Max events per page. Defaults to 25 — comfortable batch for an
    /// agent loop tick without dragging in a wall of backlog.
    #[arg(long, default_value_t = 25)]
    pub limit: i64,
}

pub async fn run(ctx: &Ctx, args: PollArgs) -> CmdResult {
    let cursor_state = CursorState::resolve(args.cursor.as_deref(), ctx);
    let effective_cursor = cursor_state.effective_cursor();

    let client = ctx.client()?;
    let page = list_events_page_tolerant(&client, effective_cursor, Some(args.limit))
        .await
        .map_err(CmdError::from)?;

    if cursor_state.persistence_enabled() {
        if let Err(e) = cursor_state.persist(page.meta.next_cursor.as_deref()) {
            warn_persist_failed(ctx, &e.to_string());
        }
    }

    for item in &page.unparseable {
        tracing::warn!(
            target: "taskfast::events",
            raw = %item.raw,
            error = %item.error,
            "unparseable event surfaced via tolerant decode"
        );
    }

    let mut seen = super::seen::SeenIds::load(ctx);
    let events: Vec<_> = page
        .events
        .iter()
        // Explicit `--cursor` is a replay: print everything, still record ids.
        .filter(|e| seen.first_time(&e.event_id.to_string()) || args.cursor.is_some())
        .collect();

    let unparseable: Vec<serde_json::Value> = page
        .unparseable
        .iter()
        .map(|u| json!({ "raw": u.raw, "error": u.error }))
        .collect();

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "events": events,
            "unparseable": unparseable,
            "meta": page.meta,
        }),
    ))
}

/// Sentinel passed via `--cursor -` to bypass the file completely.
const DISABLE_SENTINEL: &str = "-";

/// Env var that disables cursor persistence globally (read + write). Useful
/// for CI or transient debug runs that share a config dir with a real loop.
const DISABLE_ENV: &str = "TASKFAST_NO_CURSOR_STATE";

/// Carries the cursor value the request will use plus, if persistence
/// is enabled, the state store to update with `next_cursor` after the
/// poll succeeds.
struct CursorState {
    /// Cursor sent to the API on this call. None = first page.
    request_cursor: Option<String>,
    /// State store to write `next_cursor` into on success. None = no
    /// persistence (sentinel passed or env opt-out).
    store: Option<crate::state_store::StateStore>,
}

impl CursorState {
    fn resolve(cursor_arg: Option<&str>, ctx: &Ctx) -> Self {
        let env_disabled = std::env::var(DISABLE_ENV).is_ok_and(|v| v == "1");
        let sentinel = matches!(cursor_arg, Some(s) if s == DISABLE_SENTINEL);
        let store = if env_disabled || sentinel {
            None
        } else {
            Some(ctx.state_store())
        };

        let request_cursor = match cursor_arg {
            Some(s) if s == DISABLE_SENTINEL => None,
            Some(s) => Some(s.to_string()),
            None => store.as_ref().and_then(|s| s.read_cursor()),
        };

        Self {
            request_cursor,
            store,
        }
    }

    fn effective_cursor(&self) -> Option<&str> {
        self.request_cursor.as_deref()
    }

    fn persistence_enabled(&self) -> bool {
        self.store.is_some()
    }

    fn persist(&self, next_cursor: Option<&str>) -> std::io::Result<()> {
        if let Some(store) = &self.store {
            store.write_cursor(next_cursor)?;
        }
        Ok(())
    }
}

fn warn_persist_failed(ctx: &Ctx, reason: &str) {
    if ctx.quiet {
        return;
    }
    eprintln!("warning: failed to persist events cursor: {reason}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx_with_config(path: PathBuf) -> Ctx {
        let store = crate::state_store::StateStore::from_config_path(&path);
        Ctx {
            config_path: path,
            state_store: store,
            ..Default::default()
        }
    }

    #[test]
    fn dash_sentinel_disables_both_request_and_persistence() {
        let ctx = ctx_with_config(PathBuf::from("/tmp/x/.taskfast/config.json"));
        let s = CursorState::resolve(Some("-"), &ctx);
        assert!(!s.persistence_enabled());
        assert_eq!(s.effective_cursor(), None);
    }

    #[test]
    fn explicit_cursor_uses_passed_value() {
        let ctx = ctx_with_config(PathBuf::from("/tmp/x/.taskfast/config.json"));
        let s = CursorState::resolve(Some("abc"), &ctx);
        assert_eq!(s.effective_cursor(), Some("abc"));
        assert!(s.persistence_enabled());
    }

    #[test]
    fn explicit_cursor_still_sent_when_dev_null() {
        let ctx = ctx_with_config(PathBuf::from("/dev/null"));
        let s = CursorState::resolve(Some("abc"), &ctx);
        assert_eq!(s.effective_cursor(), Some("abc"));
        assert!(s.persistence_enabled());
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.json");
        let ctx = ctx_with_config(cfg);
        let s = CursorState::resolve(None, &ctx);
        s.persist(Some("xyz")).unwrap();
        let s2 = CursorState::resolve(None, &ctx);
        assert_eq!(s2.effective_cursor(), Some("xyz"));
    }

    #[test]
    fn persist_none_preserves_last_known_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.json");
        let ctx = ctx_with_config(cfg);
        let s = CursorState::resolve(None, &ctx);
        s.persist(Some("abc")).unwrap();
        s.persist(None).unwrap();
        let s2 = CursorState::resolve(None, &ctx);
        assert_eq!(s2.effective_cursor(), Some("abc"));
    }
}
