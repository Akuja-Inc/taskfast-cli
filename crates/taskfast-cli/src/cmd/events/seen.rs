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
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use super::poll::cursor_path;
use crate::cmd::Ctx;

const FILENAME: &str = "events.seen";
const CAP: usize = 10_000;

pub struct SeenIds {
    path: Option<PathBuf>,
    order: VecDeque<String>,
    set: HashSet<String>,
    /// Lines currently in the file; compact once it exceeds `2 * CAP`.
    file_lines: usize,
    quiet: bool,
    warned: bool,
}

impl SeenIds {
    pub fn load(ctx: &Ctx) -> Self {
        let path = cursor_path(ctx).map(|p| p.with_file_name(FILENAME));
        let mut s = Self {
            path,
            order: VecDeque::new(),
            set: HashSet::new(),
            file_lines: 0,
            quiet: ctx.quiet,
            warned: false,
        };
        if let Some(p) = s.path.clone() {
            match fs::read_to_string(&p) {
                Ok(raw) => {
                    for id in raw.lines().filter(|l| !l.is_empty()) {
                        s.remember(id);
                        s.file_lines += 1;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => s.warn(&format!("read {}: {e}", p.display())),
            }
        }
        s
    }

    /// Record `id`; true when it was not seen before (print it), false for
    /// a duplicate (drop it).
    pub fn first_time(&mut self, id: &str) -> bool {
        if self.set.contains(id) {
            return false;
        }
        self.remember(id);
        self.persist(id);
        true
    }

    fn remember(&mut self, id: &str) {
        if self.set.insert(id.to_string()) {
            self.order.push_back(id.to_string());
        }
        while self.order.len() > CAP {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
    }

    fn persist(&mut self, id: &str) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let res = (|| {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            if self.file_lines >= 2 * CAP {
                // ponytail: full rewrite every CAP inserts; fine at this size.
                let body: String = self.order.iter().flat_map(|i| [i.as_str(), "\n"]).collect();
                fs::write(&path, body)?;
                self.file_lines = self.order.len();
            } else {
                let mut f = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?;
                writeln!(f, "{id}")?;
                self.file_lines += 1;
            }
            Ok::<_, std::io::Error>(())
        })();
        if let Err(e) = res {
            self.warn(&format!("write {}: {e}", path.display()));
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

    fn ctx_in(dir: &std::path::Path) -> Ctx {
        Ctx {
            config_path: dir.join("config.json"),
            quiet: true,
            ..Default::default()
        }
    }

    #[test]
    fn duplicate_is_dropped_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let mut a = SeenIds::load(&ctx);
        assert!(a.first_time("e1"));
        assert!(!a.first_time("e1"));
        let mut b = SeenIds::load(&ctx);
        assert!(!b.first_time("e1"));
        assert!(b.first_time("e2"));
    }

    #[test]
    fn keeps_only_newest_cap_ids() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let mut s = SeenIds::load(&ctx);
        for i in 0..CAP + 5 {
            assert!(s.first_time(&format!("id{i}")));
        }
        assert!(s.first_time("id0"), "oldest evicted");
        assert!(!s.first_time(&format!("id{}", CAP + 4)));
        assert!(SeenIds::load(&ctx).set.len() <= CAP);
    }

    #[test]
    fn unwritable_file_warns_once_and_still_reports_first_time() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        // A directory where the file should be makes read and write fail.
        fs::create_dir(dir.path().join(FILENAME)).unwrap();
        let mut s = SeenIds::load(&ctx);
        assert!(s.warned || ctx.quiet);
        assert!(s.first_time("e1"));
        assert!(!s.first_time("e1"));
    }
}
