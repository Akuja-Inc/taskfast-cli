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
use super::intake::EventIntake;
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
    let mut intake = EventIntake::from_ctx(ctx, args.cursor.as_deref());
    let effective_cursor = intake.effective_cursor().map(str::to_string);

    let client = ctx.client()?;
    let page = list_events_page_tolerant(&client, effective_cursor.as_deref(), Some(args.limit))
        .await
        .map_err(CmdError::from)?;

    let outcome = intake.ingest_poll_page(page);

    if let Err(e) = intake.commit() {
        intake.warn(&format!("failed to persist events cursor: {e}"));
    }

    let unparseable: Vec<serde_json::Value> = outcome
        .unparseable_events
        .iter()
        .map(|u| json!({ "raw": u.raw, "error": u.error }))
        .collect();

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "events": outcome.accepted_events,
            "unparseable": unparseable,
            "meta": outcome.meta,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx_with_config(path: PathBuf) -> Ctx {
        Ctx {
            config_path: path,
            ..Default::default()
        }
    }

    #[test]
    fn dash_sentinel_disables_both_request_and_persistence() {
        let ctx = ctx_with_config(PathBuf::from("/tmp/x/.taskfast/config.json"));
        let intake = EventIntake::from_ctx(&ctx, Some("-"));
        assert!(!intake.persistence_enabled());
        assert_eq!(intake.effective_cursor(), None);
    }

    #[test]
    fn explicit_cursor_uses_passed_value() {
        let ctx = ctx_with_config(PathBuf::from("/tmp/x/.taskfast/config.json"));
        let intake = EventIntake::from_ctx(&ctx, Some("abc"));
        assert_eq!(intake.effective_cursor(), Some("abc"));
        assert!(intake.persistence_enabled());
    }

    #[test]
    fn explicit_cursor_still_sent_when_persistence_unavailable() {
        let ctx = ctx_with_config(PathBuf::from("/dev/null"));
        let intake = EventIntake::from_ctx(&ctx, Some("abc"));
        assert_eq!(intake.effective_cursor(), Some("abc"));
        assert!(!intake.persistence_enabled());
    }

    #[test]
    fn dev_null_config_disables_persistence() {
        let ctx = ctx_with_config(PathBuf::from("/dev/null"));
        let intake = EventIntake::from_ctx(&ctx, None);
        assert_eq!(intake.effective_cursor(), None);
        assert!(!intake.persistence_enabled());
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.json");
        let ctx = ctx_with_config(cfg);
        let mut intake = EventIntake::from_ctx(&ctx, None);
        assert!(intake.persistence_enabled());
        let _ = intake.ingest_batch(
            Vec::new(),
            Vec::new(),
            taskfast_client::api::types::PaginatedMeta {
                has_more: false,
                next_cursor: Some("xyz".to_string()),
                total_count: Some(0),
            },
        );
        intake.commit().unwrap();
        let intake2 = EventIntake::from_ctx(&ctx, None);
        assert_eq!(intake2.effective_cursor(), Some("xyz"));
    }

    #[test]
    fn persist_none_preserves_last_known_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.json");
        let ctx = ctx_with_config(cfg);
        let mut intake = EventIntake::from_ctx(&ctx, None);
        let _ = intake.ingest_batch(
            Vec::new(),
            Vec::new(),
            taskfast_client::api::types::PaginatedMeta {
                has_more: false,
                next_cursor: Some("abc".to_string()),
                total_count: Some(0),
            },
        );
        intake.commit().unwrap();

        let mut intake2 = EventIntake::from_ctx(&ctx, None);
        let _ = intake2.ingest_batch(
            Vec::new(),
            Vec::new(),
            taskfast_client::api::types::PaginatedMeta {
                has_more: false,
                next_cursor: None,
                total_count: Some(0),
            },
        );
        intake2.commit().unwrap();

        let intake3 = EventIntake::from_ctx(&ctx, None);
        assert_eq!(intake3.effective_cursor(), Some("abc"));
    }
}
