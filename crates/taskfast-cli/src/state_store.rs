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
        let is_taskfast_dir = Path::new(DEFAULT_CONFIG_PATH)
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|expected| dir.file_name() == Some(expected));

        if is_taskfast_dir && fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "refusing to harden symlinked state directory {}",
                    dir.display()
                ),
            ));
        }

        if is_taskfast_dir {
            #[cfg(unix)]
            {
                create_private_dir(dir)?;
            }
            #[cfg(not(unix))]
            {
                fs::create_dir_all(dir)?;
            }
            ensure_dir_gitignore(dir)?;
        } else {
            fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    /// Verify a file can be saved at `path` without creating it: hardens the
    /// parent, rejects a directory destination, and proves a sibling temp
    /// file can be created. Meant to run before any irreversible remote step.
    pub fn preflight_file(path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            Self::ensure_dir_path(parent)?;
        }
        let probe = path.with_file_name(format!(
            "{}.probe.{}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
            uuid::Uuid::new_v4().simple()
        ));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let probe_file = opts.open(&probe)?;
        let verdict = replaceable(path, &probe_file);
        drop(probe_file);
        fs::remove_file(&probe)?;
        verdict
    }

    /// [`Self::preflight_file`] for a state asset; a no-op when ephemeral.
    pub fn preflight(&self, asset: StateAsset) -> io::Result<()> {
        match self.path_for(asset) {
            Some(path) => Self::preflight_file(&path),
            None => Ok(()),
        }
    }

    /// Atomic file write: writes to sibling `.tmp`, 0600 on Unix from creation, flushes, renames.
    ///
    /// Cleans up the temporary file if write or rename fails. Hardens the parent directory only when it is named `.taskfast`.
    pub fn atomic_write_file(path: &Path, contents: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                Self::ensure_dir_path(parent)?;
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

        self.ensure_dir()?;
        let mut open = fs::OpenOptions::new();
        open.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        let mut f = open.open(&path)?;

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
        self.try_pending_fee_retry(task_id).ok().flatten()
    }

    /// Fallible lookup: only a missing ledger is empty; unreadable or corrupt state is an error.
    pub fn try_pending_fee_retry(&self, task_id: &uuid::Uuid) -> io::Result<Option<String>> {
        Ok(self.read_fee_ledger()?.get(&task_id.to_string()).cloned())
    }

    fn read_fee_ledger(&self) -> io::Result<std::collections::HashMap<String, String>> {
        let Some(path) = self.path_for(StateAsset::FeeRetries) else {
            return Ok(std::collections::HashMap::new());
        };
        match fs::metadata(&path) {
            Ok(m) if !m.is_file() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("fee retry ledger {} is not a regular file", path.display()),
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(std::collections::HashMap::new());
            }
            Err(e) => return Err(e),
            Ok(_) => {}
        }
        match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("fee retry ledger {} is unreadable: {e}", path.display()),
                )
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(std::collections::HashMap::new()),
            Err(e) => Err(e),
        }
    }

    pub fn record_pending_fee_retry(&self, task_id: &uuid::Uuid, tx_hash: &str) -> io::Result<()> {
        let Some(path) = self.path_for(StateAsset::FeeRetries) else {
            return Ok(());
        };
        let mut map = self.read_fee_ledger()?;
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
        let mut map = self.read_fee_ledger()?;
        if map.remove(&task_id.to_string()).is_some() {
            let body = serde_json::to_vec_pretty(&map)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Self::atomic_write_file(&path, &body)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let handle = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(dir)?;
    handle.set_permissions(fs::Permissions::from_mode(0o700))
}

fn replaceable(path: &Path, probe: &fs::File) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is a directory, not a file", path.display()),
        ));
    }
    if !meta.is_file() && !meta.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let me = probe.metadata()?.uid();
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        let parent_meta = fs::metadata(parent.unwrap_or_else(|| Path::new(".")))?;
        if me != 0
            && parent_meta.mode() & 0o1000 != 0
            && parent_meta.uid() != me
            && meta.uid() != me
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is owned by another user in a sticky directory and cannot be replaced",
                    path.display()
                ),
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = probe;
    Ok(())
}

fn ensure_dir_gitignore(dir: &Path) -> io::Result<()> {
    let gitignore = dir.join(".gitignore");
    match fs::symlink_metadata(&gitignore) {
        Ok(m) if m.is_file() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", gitignore.display()),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&gitignore)?;
            f.write_all(b"*\n")
        }
        Err(e) => Err(e),
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

        let seen = store.load_seen_lines().unwrap();
        assert!(seen.is_empty(), "expected no seen lines, got {seen:?}");
        store.append_seen_line("evt_1").unwrap();
        store.append_seen_line("evt_2").unwrap();
        assert_eq!(store.load_seen_lines().unwrap(), vec!["evt_1", "evt_2"]);

        let mut queue = std::collections::VecDeque::new();
        queue.push_back("evt_3".to_string());
        store.rewrite_seen_lines(&queue).unwrap();
        assert_eq!(store.load_seen_lines().unwrap(), vec!["evt_3"]);
    }

    #[cfg(unix)]
    fn assert_hardened_fresh_dir(dir: &Path, file: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let dir_mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(fs::read_to_string(dir.join(".gitignore")).unwrap(), "*\n");
        let file_mode = fs::metadata(file).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn write_cursor_into_missing_taskfast_dir_hardens_it() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".taskfast");
        let store = StateStore::from_config_path(&dir.join("config.json"));
        store.write_cursor(Some("cur1")).unwrap();
        assert_hardened_fresh_dir(&dir, &dir.join("events.cursor"));
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_seen_lines_into_missing_taskfast_dir_hardens_it() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".taskfast");
        let store = StateStore::from_config_path(&dir.join("config.json"));
        store
            .rewrite_seen_lines(&VecDeque::from(["a".to_string()]))
            .unwrap();
        assert_hardened_fresh_dir(&dir, &dir.join("events.seen"));
    }

    #[cfg(unix)]
    #[test]
    fn record_pending_fee_retry_into_missing_taskfast_dir_hardens_it() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".taskfast");
        let store = StateStore::from_config_path(&dir.join("config.json"));
        store
            .record_pending_fee_retry(&uuid::Uuid::new_v4(), "0xabc")
            .unwrap();
        assert_hardened_fresh_dir(&dir, &dir.join("fee_retries.json"));
    }

    #[cfg(unix)]
    #[test]
    fn write_through_symlinked_taskfast_dir_is_refused_and_target_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let shared = tmp.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        let link = tmp.path().join(".taskfast");
        std::os::unix::fs::symlink(&shared, &link).unwrap();
        let store = StateStore::from_config_path(&link.join("config.json"));

        assert!(store.write_cursor(Some("cur1")).is_err());

        let mode = fs::metadata(&shared).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert!(!shared.join(".gitignore").exists());
        assert!(!shared.join("events.cursor").exists());
    }

    #[cfg(unix)]
    #[test]
    fn preflight_rejects_directory_at_destination() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("fee_retries.json");
        fs::create_dir(&dest).unwrap();
        let err = StateStore::preflight_file(&dest).unwrap_err();
        assert!(err.to_string().contains("directory"), "{err}");
    }

    #[test]
    fn preflight_leaves_no_temp_file_behind() {
        let tmp = TempDir::new().unwrap();
        StateStore::preflight_file(&tmp.path().join("hook.secret")).unwrap();
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn preflight_skips_ephemeral_store() {
        let store = StateStore::ephemeral();
        assert!(store.preflight(StateAsset::FeeRetries).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn append_seen_line_through_symlinked_taskfast_dir_is_refused() {
        let tmp = TempDir::new().unwrap();
        let shared = tmp.path().join("shared");
        fs::create_dir(&shared).unwrap();
        let link = tmp.path().join(".taskfast");
        std::os::unix::fs::symlink(&shared, &link).unwrap();
        let store = StateStore::from_config_path(&link.join("config.json"));
        assert!(store.append_seen_line("evt1").is_err());
        assert!(!shared.join("events.seen").exists());
    }

    #[cfg(unix)]
    #[test]
    fn append_seen_line_hardens_existing_loose_taskfast_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".taskfast");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let store = StateStore::from_config_path(&dir.join("config.json"));
        store.append_seen_line("evt1").unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn new_taskfast_dir_is_never_wider_than_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".taskfast");
        StateStore::ensure_dir_path(&dir).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn corrupt_fee_ledger_is_an_error_and_left_untouched() {
        let tmp = TempDir::new().unwrap();
        let store = StateStore::from_config_path(&tmp.path().join("config.json"));
        let ledger = tmp.path().join("fee_retries.json");
        fs::write(&ledger, "{ not json").unwrap();
        let id = uuid::Uuid::new_v4();
        assert!(store.try_pending_fee_retry(&id).is_err());
        assert_eq!(store.get_pending_fee_retry(&id), None);
        assert!(store.record_pending_fee_retry(&id, "0xabc").is_err());
        assert!(store.clear_pending_fee_retry(&id).is_err());
        assert_eq!(fs::read_to_string(&ledger).unwrap(), "{ not json");
    }

    #[cfg(unix)]
    #[test]
    fn gitignore_that_is_a_directory_is_refused() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".taskfast");
        fs::create_dir_all(dir.join(".gitignore")).unwrap();
        let err = StateStore::ensure_dir_path(&dir).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn gitignore_that_is_a_symlink_is_refused() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".taskfast");
        fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink("/nonexistent", dir.join(".gitignore")).unwrap();
        assert!(StateStore::ensure_dir_path(&dir).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn preflight_rejects_unwritable_parent() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("ro");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let res = StateStore::preflight_file(&dir.join("hook.secret"));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(res.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn fifo_fee_ledger_is_rejected_without_blocking() {
        let tmp = TempDir::new().unwrap();
        let store = StateStore::from_config_path(&tmp.path().join("config.json"));
        let fifo = tmp.path().join("fee_retries.json");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let err = store
            .try_pending_fee_retry(&uuid::Uuid::new_v4())
            .unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    #[test]
    fn preflight_file_checks_explicit_dev_paths_instead_of_skipping() {
        if std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open("/dev/.taskfast-probe")
            .is_ok()
        {
            let _ = std::fs::remove_file("/dev/.taskfast-probe");
            return;
        }
        assert!(StateStore::preflight_file(Path::new("/dev/null")).is_err());
    }

    #[test]
    fn atomic_write_file_into_other_dir_writes_no_gitignore() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("secrets");
        StateStore::atomic_write_file(&dir.join("hook.secret"), b"s").unwrap();
        assert!(!dir.join(".gitignore").exists());
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
