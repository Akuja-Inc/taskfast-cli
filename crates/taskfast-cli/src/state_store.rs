// SPDX-License-Identifier: MIT
//! Unified local state and secret persistence for `.taskfast/` directory.
//!
//! Encapsulates path resolution, directory permissions (0700 on Unix for `.taskfast`),
//! automatic `.gitignore` provisioning, atomic file writes with 0600 permissions from
//! creation, and an ephemeral adapter for test execution with `/dev/null`.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::config::DEFAULT_CONFIG_PATH;

/// Typed assets managed within the local `.taskfast/` state directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StateAsset {
    /// CLI configuration (`config.json`).
    Config,
    /// Event stream pagination cursor (`events.cursor`).
    EventsCursor,
    /// Deduplication cache for received event IDs (`events.seen`).
    EventsSeen,
    /// Pending submission fee retry transaction hashes (`fee_retries.json`).
    FeeRetries,
}

impl StateAsset {
    /// Return the standard file name for this state asset.
    pub const fn filename(self) -> &'static str {
        match self {
            Self::Config => "config.json",
            Self::EventsCursor => "events.cursor",
            Self::EventsSeen => "events.seen",
            Self::FeeRetries => "fee_retries.json",
        }
    }
}

/// Unified state store for `.taskfast/` local files.
#[derive(Debug, Clone)]
pub struct StateStore {
    root_dir: Option<PathBuf>,
}

impl StateStore {
    /// Construct a store from a config path.
    ///
    /// When `config_path` is `/dev/null` or within `/dev/`, creates an ephemeral
    /// store that never touches the filesystem. Otherwise, creates a disk-backed
    /// store rooted at the parent directory of `config_path`.
    pub fn from_config_path(config_path: &Path) -> Self {
        if config_path == Path::new("/dev/null") || config_path.parent() == Some(Path::new("/dev"))
        {
            return Self::ephemeral();
        }

        let root_dir = config_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        Self {
            root_dir: Some(root_dir),
        }
    }

    /// Construct an ephemeral store with no on-disk persistence.
    pub fn ephemeral() -> Self {
        Self { root_dir: None }
    }

    /// Return true if this store is ephemeral (no on-disk persistence).
    pub fn is_ephemeral(&self) -> bool {
        self.root_dir.is_none()
    }

    /// Return the root directory if disk-backed.
    pub fn root_dir(&self) -> Option<&Path> {
        self.root_dir.as_deref()
    }

    /// Return the resolved path on disk for a state asset, or None if ephemeral.
    pub fn path_for(&self, asset: StateAsset) -> Option<PathBuf> {
        self.root_dir.as_ref().map(|dir| dir.join(asset.filename()))
    }

    /// Ensure the root directory exists with 0700 permissions and automatic `.gitignore`.
    pub fn ensure_dir(&self) -> io::Result<()> {
        if let Some(dir) = &self.root_dir {
            Self::ensure_dir_path(dir)?;
        }
        Ok(())
    }

    /// Ensure a directory exists. When the directory is named `.taskfast`, tightens
    /// permissions to 0700 on Unix and provisions `.gitignore`.
    pub fn ensure_dir_path(dir: &Path) -> io::Result<()> {
        if dir.as_os_str().is_empty() || dir == Path::new(".") {
            return Ok(());
        }
        fs::create_dir_all(dir)?;

        let is_taskfast_dir = Path::new(DEFAULT_CONFIG_PATH)
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|expected| dir.file_name() == Some(expected));

        if is_taskfast_dir {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            }
            ensure_dir_gitignore(dir);
        }
        Ok(())
    }

    /// Atomic file write: writes to sibling `.tmp`, 0600 on Unix from creation, flushes, renames.
    ///
    /// Cleans up the temporary file if write or rename fails. Does not chmod the parent directory.
    pub fn atomic_write_file(path: &Path, contents: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }

        let tmp_name = format!(
            "{}.tmp.{}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
            uuid::Uuid::new_v4().simple()
        );
        let tmp = path.with_file_name(tmp_name);

        let write_res = (|| -> io::Result<()> {
            #[cfg(unix)]
            let create = {
                use std::os::unix::fs::OpenOptionsExt;
                fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)
            };
            #[cfg(not(unix))]
            let create = fs::File::create(&tmp);

            let mut f = create?;
            f.write_all(contents)?;
            f.flush()?;
            drop(f);
            fs::rename(&tmp, path)?;
            Ok(())
        })();

        if write_res.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        write_res
    }

    /// Read the pagination cursor if present and non-empty.
    pub fn read_cursor(&self) -> Option<String> {
        let path = self.path_for(StateAsset::EventsCursor)?;
        let raw = fs::read_to_string(path).ok()?;
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    /// Write pagination cursor. A `None` cursor leaves the existing cursor untouched.
    pub fn write_cursor(&self, next_cursor: Option<&str>) -> io::Result<()> {
        let Some(payload) = next_cursor else {
            return Ok(());
        };
        let Some(path) = self.path_for(StateAsset::EventsCursor) else {
            return Ok(());
        };
        Self::atomic_write_file(&path, payload.as_bytes())
    }

    /// Load seen event IDs from `events.seen`.
    pub fn load_seen_lines(&self) -> io::Result<Vec<String>> {
        let Some(path) = self.path_for(StateAsset::EventsSeen) else {
            return Ok(Vec::new());
        };
        match fs::read_to_string(path) {
            Ok(raw) => Ok(raw
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Append a single seen ID to `events.seen`.
    pub fn append_seen_line(&self, id: &str) -> io::Result<()> {
        let Some(path) = self.path_for(StateAsset::EventsSeen) else {
            return Ok(());
        };

        let open_append = || {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .open(&path)
            }
            #[cfg(not(unix))]
            {
                fs::OpenOptions::new().create(true).append(true).open(&path)
            }
        };

        let mut f = match open_append() {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.ensure_dir()?;
                open_append()?
            }
            Err(e) => return Err(e),
        };

        writeln!(f, "{id}")?;
        Ok(())
    }

    /// Rewrite `events.seen` with the given IDs, separated by newlines.
    pub fn rewrite_seen_lines(&self, ids: &VecDeque<String>) -> io::Result<()> {
        let Some(path) = self.path_for(StateAsset::EventsSeen) else {
            return Ok(());
        };
        let mut body = String::new();
        for id in ids {
            body.push_str(id);
            body.push('\n');
        }
        Self::atomic_write_file(&path, body.as_bytes())
    }

    /// Read any pending fee retry transaction hash for `task_id`.
    pub fn get_pending_fee_retry(&self, task_id: &uuid::Uuid) -> Option<String> {
        let path = self.path_for(StateAsset::FeeRetries)?;
        let raw = fs::read_to_string(path).ok()?;
        let map: std::collections::HashMap<String, String> = serde_json::from_str(&raw).ok()?;
        map.get(&task_id.to_string()).cloned()
    }

    /// Record a pending fee retry transaction hash for `task_id`.
    pub fn record_pending_fee_retry(&self, task_id: &uuid::Uuid, tx_hash: &str) -> io::Result<()> {
        let Some(path) = self.path_for(StateAsset::FeeRetries) else {
            return Ok(());
        };
        let mut map: std::collections::HashMap<String, String> = if path.exists() {
            fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str(&raw).ok())
                .unwrap_or_default()
        } else {
            std::collections::HashMap::new()
        };
        map.insert(task_id.to_string(), tx_hash.to_string());
        let body = serde_json::to_vec_pretty(&map)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Self::atomic_write_file(&path, &body)
    }

    /// Clear any pending fee retry transaction hash for `task_id`.
    pub fn clear_pending_fee_retry(&self, task_id: &uuid::Uuid) -> io::Result<()> {
        let Some(path) = self.path_for(StateAsset::FeeRetries) else {
            return Ok(());
        };
        if !path.exists() {
            return Ok(());
        }
        let raw = fs::read_to_string(&path)?;
        let mut map: std::collections::HashMap<String, String> =
            serde_json::from_str(&raw).unwrap_or_default();
        if map.remove(&task_id.to_string()).is_some() {
            let body = serde_json::to_vec_pretty(&map)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Self::atomic_write_file(&path, &body)?;
        }
        Ok(())
    }
}

fn ensure_dir_gitignore(dir: &Path) {
    let gitignore = dir.join(".gitignore");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&gitignore)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(b"*\n") {
                tracing::warn!(path = %gitignore.display(), error = %e, "could not write .taskfast/.gitignore");
            }
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            tracing::warn!(path = %gitignore.display(), error = %e, "could not create .taskfast/.gitignore");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn ephemeral_mode_skips_disk() {
        let store = StateStore::from_config_path(Path::new("/dev/null"));
        assert!(store.is_ephemeral());
        assert_eq!(store.root_dir(), None);
        assert_eq!(store.path_for(StateAsset::Config), None);

        assert!(store.write_cursor(Some("cur1")).is_ok());
        assert_eq!(store.read_cursor(), None);
    }

    #[test]
    fn disk_mode_creates_0700_dir_and_gitignore() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join(".taskfast").join("config.json");
        let store = StateStore::from_config_path(&config_path);
        assert!(!store.is_ephemeral());

        store.ensure_dir().unwrap();
        let root = store.root_dir().unwrap();
        assert!(root.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(root).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "directory mode must be 0700");
        }

        let gitignore = root.join(".gitignore");
        assert!(
            gitignore.exists(),
            ".gitignore must be created for .taskfast"
        );
        let content = fs::read_to_string(gitignore).unwrap();
        assert_eq!(content, "*\n");
    }

    #[test]
    fn atomic_write_file_sets_0600_from_creation_without_chmod_parent() {
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path().join("sub");
        let target = parent.join("secret.file");
        StateStore::atomic_write_file(&target, b"secret-data").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"secret-data");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600, "file mode must be 0600");

            // Parent directory must not be chmod'd to 0700 when writing arbitrary file
            let parent_mode = fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
            assert_ne!(parent_mode, 0o700, "parent directory must not be 0700");
        }
    }

    #[test]
    fn cursor_and_seen_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.json");
        let store = StateStore::from_config_path(&config_path);

        assert_eq!(store.read_cursor(), None);
        store.write_cursor(Some("c1")).unwrap();
        assert_eq!(store.read_cursor(), Some("c1".into()));
        store.write_cursor(None).unwrap();
        assert_eq!(store.read_cursor(), Some("c1".into()));

        assert!(store.load_seen_lines().unwrap().is_empty());
        store.append_seen_line("evt_1").unwrap();
        store.append_seen_line("evt_2").unwrap();
        assert_eq!(store.load_seen_lines().unwrap(), vec!["evt_1", "evt_2"]);

        let mut queue = std::collections::VecDeque::new();
        queue.push_back("evt_3".to_string());
        store.rewrite_seen_lines(&queue).unwrap();
        assert_eq!(store.load_seen_lines().unwrap(), vec!["evt_3"]);
    }

    #[test]
    fn pending_fee_retry_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.json");
        let store = StateStore::from_config_path(&config_path);
        let task_id = uuid::Uuid::new_v4();

        assert_eq!(store.get_pending_fee_retry(&task_id), None);
        store
            .record_pending_fee_retry(&task_id, "0xabcdef123456")
            .unwrap();
        assert_eq!(
            store.get_pending_fee_retry(&task_id),
            Some("0xabcdef123456".into())
        );

        store.clear_pending_fee_retry(&task_id).unwrap();
        assert_eq!(store.get_pending_fee_retry(&task_id), None);
    }
}
