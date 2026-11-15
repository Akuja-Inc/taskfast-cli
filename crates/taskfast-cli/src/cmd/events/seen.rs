// SPDX-License-Identifier: MIT
//! Seen-event-id set for `events poll` / `events stream` (gh#204).
//!
//! The server delivers at least once, so the same `event_id` can arrive
//! twice. The newest 10,000 ids live in one file next to the config
//! (`events.seen`, one id per line, oldest first). The server retries for
//! about 6 minutes, so 10,000 ids covers every retry with a wide margin.
//!
//! I/O failures never drop events: warn once on stderr, keep going with
//! whatever is in memory (at-least-once beats losing an event).

use std::collections::{HashSet, VecDeque};

use crate::state_store::{StateAsset, StateStore};

const CAP: usize = 10_000;

pub struct SeenIds {
    store: StateStore,
    order: VecDeque<String>,
    set: HashSet<String>,
    /// Lines currently in the file; compact once it exceeds `2 * CAP`.
    file_lines: usize,
    quiet: bool,
    warned: bool,
}

impl SeenIds {
    pub fn new(store: StateStore, quiet: bool) -> Self {
        let mut s = Self {
            store: store.clone(),
            order: VecDeque::new(),
            set: HashSet::new(),
            file_lines: 0,
            quiet,
            warned: false,
        };
        match store.load_seen_lines() {
            Ok(lines) => {
                for id in lines {
                    s.remember(&id);
                    s.file_lines += 1;
                }
            }
            Err(e) => {
                let display = store
                    .path_for(StateAsset::EventsSeen)
                    .map_or_else(|| "in-memory".to_string(), |p| p.display().to_string());
                s.warn(&format!("read {display}: {e}"));
            }
        }
        s
    }

    /// Check if `id` has already been seen.
    pub fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }

    pub fn remember(&mut self, id: &str) {
        if self.set.insert(id.to_string()) {
            self.order.push_back(id.to_string());
        }
        while self.order.len() > CAP {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
    }

    /// Commit a batch of seen IDs into memory and persist to StateStore.
    pub fn commit_ids(&mut self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }

        for id in ids {
            self.remember(id);
        }

        let res = if self.file_lines + ids.len() >= 2 * CAP {
            let res = self.store.rewrite_seen_lines(&self.order);
            if res.is_ok() {
                self.file_lines = self.order.len();
            }
            res
        } else {
            let mut write_err = Ok(());
            for id in ids {
                if let Err(e) = self.store.append_seen_line(id) {
                    write_err = Err(e);
                    break;
                }
                self.file_lines += 1;
            }
            write_err
        };
        if let Err(e) = res {
            let display = self
                .store
                .path_for(StateAsset::EventsSeen)
                .map_or_else(|| "in-memory".to_string(), |p| p.display().to_string());
            self.warn(&format!("write {display}: {e}"));
        }
    }

    fn warn(&mut self, msg: &str) {
        if self.warned || self.quiet {
            return;
        }
        self.warned = true;
        eprintln!("warning: events seen-id file unusable ({msg}); duplicates may be printed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(dir: &std::path::Path) -> StateStore {
        StateStore::from_config_path(&dir.join("config.json"))
    }

    fn record_id(s: &mut SeenIds, id: &str) -> bool {
        if s.contains(id) {
            return false;
        }
        s.commit_ids(&[id.to_string()]);
        true
    }

    #[test]
    fn duplicate_is_dropped_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let mut a = SeenIds::new(store.clone(), true);
        assert!(record_id(&mut a, "e1"));
        assert!(!record_id(&mut a, "e1"));
        let mut b = SeenIds::new(store, true);
        assert!(!record_id(&mut b, "e1"));
        assert!(record_id(&mut b, "e2"));
    }

    #[test]
    fn keeps_only_newest_cap_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let mut s = SeenIds::new(store.clone(), true);
        for i in 0..CAP + 5 {
            assert!(record_id(&mut s, &format!("id{i}")));
        }
        assert!(record_id(&mut s, "id0"), "oldest evicted");
        assert!(!record_id(&mut s, &format!("id{}", CAP + 4)));
        assert!(SeenIds::new(store, true).set.len() <= CAP);
    }

    #[test]
    fn unwritable_file_warns_once_and_still_reports_first_time() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        // A directory where the file should be makes read and write fail.
        std::fs::create_dir(dir.path().join(StateAsset::EventsSeen.filename())).unwrap();
        let mut s = SeenIds::new(store, true);
        assert!(s.warned || s.quiet);
        assert!(record_id(&mut s, "e1"));
        assert!(!record_id(&mut s, "e1"));
    }
}
