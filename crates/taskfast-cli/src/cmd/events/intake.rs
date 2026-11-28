// SPDX-License-Identifier: MIT
//! Deep lifecycle event intake module (gh#217).
//!
//! Encapsulates:
//! - Tolerant per-item JSON decoding with metrics/counters for schema drift.
//! - Bounded [`SeenIds`] ring-buffer deduplication across reconnects and poll iterations.
//! - Coordinated cursor progression and seen-id commitment after successful batch processing (integrating with [`StateStore`]).
//! - A clean seam accepting incoming event batches from both HTTP polling and WebSocket streaming transports.

use std::io;

use serde_json::Value;
use taskfast_agent::events::{decode_event_item, TolerantEventPage, UnparseableEvent};
use taskfast_client::api::types::{AgentEvent, PaginatedMeta};

use super::seen::SeenIds;
use crate::cmd::Ctx;
use crate::state_store::StateStore;

/// Sentinel passed via `--cursor -` to bypass the file completely.
pub const DISABLE_SENTINEL: &str = "-";

/// Env var that disables cursor persistence globally (read + write). Useful
/// for CI or transient debug runs that share a config dir with a real loop.
pub const DISABLE_ENV: &str = "TASKFAST_NO_CURSOR_STATE";

/// Metrics and counters for schema drift and event intake.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IntakeMetrics {
    /// Total events received across poll and stream intakes.
    pub total_received: u64,
    /// Successfully accepted events.
    pub accepted_events: u64,
    /// Events dropped due to ring-buffer deduplication.
    pub deduplicated_events: u64,
    /// Events that failed item-level decoding due to schema drift.
    pub unparseable_events: u64,
}

/// Outcome of ingesting a batch of events.
#[derive(Debug, Clone)]
pub struct IngestBatchOutcome {
    pub accepted_events: Vec<AgentEvent>,
    pub unparseable_events: Vec<UnparseableEvent>,
    pub meta: PaginatedMeta,
}

/// Outcome of ingesting a single stream payload.
#[derive(Debug, Clone)]
pub enum StreamItemOutcome {
    Accepted { event_id: String },
    Duplicate { event_id: String },
    Malformed { error: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum PendingCursor {
    #[default]
    None,
    Update(Option<String>),
}

/// Unified event intake orchestrator.
#[allow(clippy::struct_excessive_bools)]
pub struct EventIntake {
    store: StateStore,
    seen: SeenIds,
    request_cursor: Option<String>,
    persistence_enabled: bool,
    replay_mode: bool,
    metrics: IntakeMetrics,
    pending_seen: Vec<String>,
    pending_cursor: PendingCursor,
    quiet: bool,
    warned: bool,
}

impl EventIntake {
    /// Construct an intake instance from CLI context and optional `--cursor` flag.
    pub fn from_ctx(ctx: &Ctx, cursor_arg: Option<&str>) -> Self {
        let store = ctx.state_store();
        Self::new(store, cursor_arg, ctx.quiet)
    }

    /// Construct an intake instance with explicit store, cursor argument, and quiet flag.
    pub fn new(store: StateStore, cursor_arg: Option<&str>, quiet: bool) -> Self {
        let env_disabled = std::env::var(DISABLE_ENV).is_ok_and(|v| v == "1");
        let sentinel = matches!(cursor_arg, Some(s) if s == DISABLE_SENTINEL);
        let persistence_enabled = !env_disabled && !sentinel && !store.is_ephemeral();

        let replay_mode = cursor_arg.is_some();

        let request_cursor = match cursor_arg {
            Some(s) if s == DISABLE_SENTINEL => None,
            Some(s) => Some(s.to_string()),
            None => {
                if persistence_enabled {
                    store.read_cursor()
                } else {
                    None
                }
            }
        };

        let seen = SeenIds::new(store.clone(), quiet);

        Self {
            store,
            seen,
            request_cursor,
            persistence_enabled,
            replay_mode,
            metrics: IntakeMetrics::default(),
            pending_seen: Vec::new(),
            pending_cursor: PendingCursor::None,
            quiet,
            warned: false,
        }
    }

    /// Effective cursor to use for request pagination.
    pub fn effective_cursor(&self) -> Option<&str> {
        self.request_cursor.as_deref()
    }

    /// Whether cursor and seen persistence are active.
    pub fn persistence_enabled(&self) -> bool {
        self.persistence_enabled
    }

    /// Whether intake is operating in replay mode (explicit cursor passed).
    pub fn replay_mode(&self) -> bool {
        self.replay_mode
    }

    /// Snapshot of intake metrics.
    pub fn metrics(&self) -> IntakeMetrics {
        self.metrics
    }

    /// Ingest a tolerant event page from an HTTP poll response.
    pub fn ingest_poll_page(&mut self, page: TolerantEventPage) -> IngestBatchOutcome {
        self.ingest_batch(page.events, page.unparseable, page.meta)
    }

    /// Ingest a batch of typed events, unparseable items, and pagination metadata.
    pub fn ingest_batch(
        &mut self,
        events: Vec<AgentEvent>,
        unparseable: Vec<UnparseableEvent>,
        meta: PaginatedMeta,
    ) -> IngestBatchOutcome {
        for u in &unparseable {
            self.metrics.unparseable_events += 1;
            tracing::warn!(
                target: "taskfast::events",
                raw = %u.raw,
                error = %u.error,
                "unparseable event surfaced via tolerant decode"
            );
        }

        let mut accepted = Vec::new();
        for ev in events {
            self.metrics.total_received += 1;
            let id = ev.event_id.to_string();
            let is_duplicate = self.seen.contains(&id);

            if is_duplicate && !self.replay_mode {
                self.metrics.deduplicated_events += 1;
            } else {
                self.metrics.accepted_events += 1;
                self.seen.remember(&id);
                self.pending_seen.push(id);
                accepted.push(ev);
            }
        }

        self.pending_cursor = PendingCursor::Update(meta.next_cursor.clone());

        IngestBatchOutcome {
            accepted_events: accepted,
            unparseable_events: unparseable,
            meta,
        }
    }

    /// Ingest a single WebSocket event payload.
    pub fn ingest_stream_payload(&mut self, payload: &Value) -> StreamItemOutcome {
        self.metrics.total_received += 1;
        let Some(event_id) = payload.get("event_id").and_then(Value::as_str) else {
            self.metrics.unparseable_events += 1;
            return StreamItemOutcome::Malformed {
                error: "missing event_id in event payload".to_string(),
            };
        };

        if self.seen.contains(event_id) {
            self.metrics.deduplicated_events += 1;
            return StreamItemOutcome::Duplicate {
                event_id: event_id.to_string(),
            };
        }

        if let Err(u) = decode_event_item(payload.clone()) {
            self.metrics.unparseable_events += 1;
            tracing::warn!(
                target: "taskfast::events",
                event_id = %event_id,
                error = %u.error,
                "schema drift in lifecycle event payload"
            );
        } else {
            self.metrics.accepted_events += 1;
        }

        StreamItemOutcome::Accepted {
            event_id: event_id.to_string(),
        }
    }

    pub fn mark_stream_event_seen(&mut self, event_id: &str) {
        if self.persistence_enabled {
            self.seen.commit_ids(&[event_id.to_string()]);
        } else {
            self.seen.remember(event_id);
        }
    }

    /// Record an unparseable non-JSON text frame.
    pub fn record_unparseable(&mut self) {
        self.metrics.unparseable_events += 1;
    }

    /// Commit cursor progression and seen-id deduplication state to StateStore.
    pub fn commit(&mut self) -> io::Result<()> {
        if self.persistence_enabled {
            if let PendingCursor::Update(ref cursor_opt) = self.pending_cursor {
                self.store.write_cursor(cursor_opt.as_deref())?;
            }
            self.pending_cursor = PendingCursor::None;
            if !self.pending_seen.is_empty() {
                self.seen.commit_ids(&self.pending_seen);
                self.pending_seen.clear();
            }
        } else {
            for id in self.pending_seen.drain(..) {
                self.seen.remember(&id);
            }
            self.pending_cursor = PendingCursor::None;
        }
        Ok(())
    }

    /// Print a single warning to stderr if not in quiet mode.
    pub fn warn(&mut self, msg: &str) {
        if self.warned || self.quiet {
            return;
        }
        self.warned = true;
        eprintln!("warning: {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample_event(id: &str, name: &str) -> AgentEvent {
        serde_json::from_value(json!({
            "event_id": id,
            "event": name,
            "occurred_at": "2026-11-15T22:00:00Z",
            "data": {},
        }))
        .unwrap()
    }

    #[test]
    fn in_memory_cursor_and_seen_deduplication() {
        let tmp = TempDir::new().unwrap();
        let store = StateStore::from_config_path(&tmp.path().join("config.json"));
        let mut intake = EventIntake::new(store.clone(), None, true);

        assert_eq!(intake.effective_cursor(), None);

        let ev1 = sample_event("00000000-0000-0000-0000-000000000001", "task_assigned");
        let ev2 = sample_event("00000000-0000-0000-0000-000000000002", "bid_placed");

        let outcome = intake.ingest_batch(
            vec![ev1.clone(), ev2.clone()],
            Vec::new(),
            PaginatedMeta {
                has_more: true,
                next_cursor: Some("cur_1".to_string()),
                total_count: Some(2),
            },
        );

        assert_eq!(outcome.accepted_events.len(), 2);
        assert_eq!(store.read_cursor(), None);

        intake.commit().unwrap();
        assert_eq!(store.read_cursor(), Some("cur_1".to_string()));

        let ev3 = sample_event("00000000-0000-0000-0000-000000000003", "task_completed");
        let outcome2 = intake.ingest_batch(
            vec![ev2.clone(), ev3.clone()],
            Vec::new(),
            PaginatedMeta {
                has_more: false,
                next_cursor: Some("cur_2".to_string()),
                total_count: Some(2),
            },
        );

        assert_eq!(outcome2.accepted_events.len(), 1);
        assert_eq!(outcome2.accepted_events[0].event_id, ev3.event_id);
        assert_eq!(intake.metrics().deduplicated_events, 1);

        intake.commit().unwrap();
        assert_eq!(store.read_cursor(), Some("cur_2".to_string()));

        let mut intake_reconnect = EventIntake::new(store, None, true);
        assert_eq!(intake_reconnect.effective_cursor(), Some("cur_2"));

        let outcome3 = intake_reconnect.ingest_batch(
            vec![ev3],
            Vec::new(),
            PaginatedMeta {
                has_more: false,
                next_cursor: None,
                total_count: Some(1),
            },
        );
        assert_eq!(outcome3.accepted_events.len(), 0);
        assert_eq!(intake_reconnect.metrics().deduplicated_events, 1);
    }

    #[test]
    fn within_batch_duplicate_events_are_deduplicated() {
        let store = StateStore::ephemeral();
        let mut intake = EventIntake::new(store, None, true);

        let ev = sample_event("00000000-0000-0000-0000-000000000001", "task_assigned");
        let outcome = intake.ingest_batch(
            vec![ev.clone(), ev],
            Vec::new(),
            PaginatedMeta {
                has_more: false,
                next_cursor: None,
                total_count: Some(2),
            },
        );

        assert_eq!(outcome.accepted_events.len(), 1);
        assert_eq!(intake.metrics().deduplicated_events, 1);
        assert_eq!(intake.metrics().accepted_events, 1);
    }

    #[test]
    fn dash_sentinel_replays_all_seen_events() {
        let tmp = TempDir::new().unwrap();
        let store = StateStore::from_config_path(&tmp.path().join("config.json"));

        let mut init_intake = EventIntake::new(store.clone(), None, true);
        let ev = sample_event("00000000-0000-0000-0000-000000000001", "task_assigned");
        init_intake.ingest_batch(
            vec![ev.clone()],
            Vec::new(),
            PaginatedMeta {
                has_more: false,
                next_cursor: Some("c1".to_string()),
                total_count: Some(1),
            },
        );
        init_intake.commit().unwrap();

        // One-shot inspection with `--cursor -` against populated seen-file
        let mut sentinel_intake = EventIntake::new(store, Some(DISABLE_SENTINEL), true);
        assert_eq!(sentinel_intake.effective_cursor(), None);
        assert!(!sentinel_intake.persistence_enabled());
        assert!(sentinel_intake.replay_mode());

        let outcome = sentinel_intake.ingest_batch(
            vec![ev],
            Vec::new(),
            PaginatedMeta {
                has_more: false,
                next_cursor: None,
                total_count: Some(1),
            },
        );
        assert_eq!(
            outcome.accepted_events.len(),
            1,
            "sentinel must replay seen events"
        );
    }

    #[test]
    fn uncommitted_batch_preserves_previous_state() {
        let tmp = TempDir::new().unwrap();
        let store = StateStore::from_config_path(&tmp.path().join("config.json"));
        store.write_cursor(Some("initial_cursor")).unwrap();

        let mut intake = EventIntake::new(store.clone(), None, true);
        assert_eq!(intake.effective_cursor(), Some("initial_cursor"));

        let ev = sample_event("00000000-0000-0000-0000-000000000001", "task_assigned");
        let _ = intake.ingest_batch(
            vec![ev],
            Vec::new(),
            PaginatedMeta {
                has_more: false,
                next_cursor: Some("aborted_cursor".to_string()),
                total_count: Some(1),
            },
        );

        drop(intake);

        let resumed = EventIntake::new(store, None, true);
        assert_eq!(resumed.effective_cursor(), Some("initial_cursor"));
        assert!(!resumed
            .seen
            .contains("00000000-0000-0000-0000-000000000001"));
    }

    #[test]
    fn stream_payload_deduplication_and_drift_handling() {
        let tmp = TempDir::new().unwrap();
        let store = StateStore::from_config_path(&tmp.path().join("config.json"));
        let mut intake = EventIntake::new(store, None, true);

        let payload = json!({
            "event_id": "00000000-0000-0000-0000-000000000001",
            "event": "task_assigned",
            // Has timestamp instead of occurred_at (schema drift)
            "timestamp": "2026-11-15T22:00:00Z",
            "data": {},
        });

        match intake.ingest_stream_payload(&payload) {
            StreamItemOutcome::Accepted { event_id } => {
                assert_eq!(event_id, "00000000-0000-0000-0000-000000000001");
                intake.mark_stream_event_seen(&event_id);
            }
            other => panic!("expected Accepted, got {other:?}"),
        }

        assert_eq!(intake.metrics().unparseable_events, 1);
        assert_eq!(intake.metrics().accepted_events, 0);

        match intake.ingest_stream_payload(&payload) {
            StreamItemOutcome::Duplicate { event_id } => {
                assert_eq!(event_id, "00000000-0000-0000-0000-000000000001");
            }
            other => panic!("expected Duplicate, got {other:?}"),
        }

        assert_eq!(intake.metrics().deduplicated_events, 1);
    }

    #[test]
    fn dash_sentinel_disables_persistence() {
        let tmp = TempDir::new().unwrap();
        let store = StateStore::from_config_path(&tmp.path().join("config.json"));
        let intake = EventIntake::new(store, Some(DISABLE_SENTINEL), true);

        assert!(!intake.persistence_enabled());
        assert_eq!(intake.effective_cursor(), None);
    }

    #[test]
    fn replay_mode_accepts_duplicates_and_records_ids() {
        let store = StateStore::ephemeral();
        let mut intake = EventIntake::new(store, Some("explicit_replay_cursor"), true);
        assert!(intake.replay_mode());

        let ev = sample_event("00000000-0000-0000-0000-000000000001", "task_assigned");

        let o1 = intake.ingest_batch(
            vec![ev.clone()],
            Vec::new(),
            PaginatedMeta {
                has_more: false,
                next_cursor: None,
                total_count: Some(1),
            },
        );
        assert_eq!(o1.accepted_events.len(), 1);

        intake.commit().unwrap();

        let o2 = intake.ingest_batch(
            vec![ev],
            Vec::new(),
            PaginatedMeta {
                has_more: false,
                next_cursor: None,
                total_count: Some(1),
            },
        );
        assert_eq!(o2.accepted_events.len(), 1);
    }

    #[test]
    fn commit_cursor_write_failure_preserves_pending_state() {
        let tmp = TempDir::new().unwrap();
        let store = StateStore::from_config_path(&tmp.path().join("config.json"));
        // Make the cursor path a directory so writing fails
        std::fs::create_dir_all(
            store
                .path_for(crate::state_store::StateAsset::EventsCursor)
                .unwrap(),
        )
        .unwrap();

        let mut intake = EventIntake::new(store, None, true);
        let ev = sample_event("00000000-0000-0000-0000-000000000001", "task_assigned");
        intake.ingest_batch(
            vec![ev],
            Vec::new(),
            PaginatedMeta {
                has_more: false,
                next_cursor: Some("c_fail".to_string()),
                total_count: Some(1),
            },
        );

        assert!(intake.commit().is_err());
        assert_eq!(intake.pending_seen.len(), 1);
        assert!(matches!(intake.pending_cursor, PendingCursor::Update(_)));
    }
}
