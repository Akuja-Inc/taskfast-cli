// SPDX-License-Identifier: MIT
//! Unified local state and secret persistence for `.taskfast/` directory.
//!
//! Encapsulates path resolution, directory permissions (0700 on Unix), automatic
//! `.gitignore` provisioning, atomic file writes with 0600 permissions from creation,
//! and an ephemeral in-memory adapter for test execution with `/dev/null`.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::{
    Config, ConfigError, CURRENT_SCHEMA_VERSION, DEFAULT_CONFIG_PATH, LEGACY_REMOVED_KEYS,
};

/// Typed assets managed within the local `.taskfast/` state directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StateAsset {
    /// CLI configuration (`config.json`).
    Config,
    /// Webhook signing secret (`hook.secret`).
    HookSecret,
    /// Event stream pagination cursor (`events.cursor`).
    EventsCursor,
    /// Deduplication cache for received event IDs (`events.seen`).
    EventsSeen,
}

impl StateAsset {
    /// Return the standard file name for this state asset.
    pub const fn filename(self) -> &'static str {
        match self {
            Self::Config => "config.json",
            Self::HookSecret => "hook.secret",
            Self::EventsCursor => "events.cursor",
            Self::EventsSeen => "events.seen",
        }
    }
}

#[derive(Debug, Clone)]
enum StateBackend {
    Disk {
        root_dir: PathBuf,
        config_path: PathBuf,
    },
    Memory {
        assets: Arc<Mutex<HashMap<StateAsset, Vec<u8>>>>,
    },
}

/// Unified state store for `.taskfast/` local files.
#[derive(Debug, Clone)]
pub struct StateStore {
    backend: StateBackend,
}

impl StateStore {
    /// Construct a store from a config path.
    ///
    /// When `config_path` is `/dev/null` or within `/dev/`, creates an ephemeral
    /// in-memory store that never touches the filesystem. Otherwise, creates a
    /// disk-backed store rooted at the parent directory of `config_path`.
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
            backend: StateBackend::Disk {
                root_dir,
                config_path: config_path.to_path_buf(),
            },
        }
    }

    /// Construct an ephemeral in-memory store.
    pub fn ephemeral() -> Self {
        Self {
            backend: StateBackend::Memory {
                assets: Arc::new(Mutex::new(HashMap::new())),
            },
        }
    }

    /// Return true if this store is ephemeral in-memory.
    pub fn is_ephemeral(&self) -> bool {
        matches!(self.backend, StateBackend::Memory { .. })
    }

    /// Return the root directory if disk-backed.
    pub fn root_dir(&self) -> Option<&Path> {
        match &self.backend {
            StateBackend::Disk { root_dir, .. } => Some(root_dir.as_path()),
            StateBackend::Memory { .. } => None,
        }
    }

    /// Return the config path if disk-backed.
    pub fn config_path(&self) -> Option<&Path> {
        match &self.backend {
            StateBackend::Disk { config_path, .. } => Some(config_path.as_path()),
            StateBackend::Memory { .. } => None,
        }
    }

    /// Return the resolved path on disk for a state asset, or None if ephemeral.
    pub fn path_for(&self, asset: StateAsset) -> Option<PathBuf> {
        match &self.backend {
            StateBackend::Disk {
                root_dir,
                config_path,
            } => match asset {
                StateAsset::Config => Some(config_path.clone()),
                _ => Some(root_dir.join(asset.filename())),
            },
            StateBackend::Memory { .. } => None,
        }
    }

    /// Ensure the root directory exists with 0700 permissions and automatic `.gitignore`.
    pub fn ensure_dir(&self) -> io::Result<()> {
        let StateBackend::Disk { root_dir, .. } = &self.backend else {
            return Ok(());
        };
        Self::ensure_dir_path(root_dir)
    }

    /// Ensure a directory exists with 0700 permissions on Unix and `.gitignore` if named `.taskfast`.
    pub fn ensure_dir_path(dir: &Path) -> io::Result<()> {
        if dir.as_os_str().is_empty() || dir == Path::new(".") {
            return Ok(());
        }
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        ensure_dir_gitignore(dir);
        Ok(())
    }

    /// Atomic file write: writes to sibling `.tmp`, 0600 on Unix from creation, flushes, renames.
    ///
    /// Cleans up the temporary file if write or rename fails.
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

    /// Read raw bytes for an asset. Missing file returns `Ok(None)`.
    pub fn read_asset(&self, asset: StateAsset) -> io::Result<Option<Vec<u8>>> {
        match &self.backend {
            StateBackend::Memory { assets } => {
                let map = assets.lock().unwrap();
                Ok(map.get(&asset).cloned())
            }
            StateBackend::Disk { .. } => {
                let Some(path) = self.path_for(asset) else {
                    return Ok(None);
                };
                match fs::read(&path) {
                    Ok(b) => Ok(Some(b)),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// Read an asset as UTF-8 string. Missing file returns `Ok(None)`.
    pub fn read_asset_string(&self, asset: StateAsset) -> io::Result<Option<String>> {
        self.read_asset(asset)
            .map(|opt| opt.map(|bytes| String::from_utf8_lossy(&bytes).to_string()))
    }

    /// Write bytes atomically to an asset slot.
    pub fn write_asset_atomic(&self, asset: StateAsset, contents: &[u8]) -> io::Result<()> {
        match &self.backend {
            StateBackend::Memory { assets } => {
                let mut map = assets.lock().unwrap();
                map.insert(asset, contents.to_vec());
                Ok(())
            }
            StateBackend::Disk { .. } => {
                let path = self.path_for(asset).expect("disk backend has path");
                Self::atomic_write_file(&path, contents)
            }
        }
    }

    /// Parse configuration from the store. Missing file returns default config.
    pub fn load_config(&self) -> Result<Config, ConfigError> {
        let display_path = self
            .config_path()
            .map_or_else(|| PathBuf::from("/dev/null"), Path::to_path_buf);

        let bytes = match self.read_asset(StateAsset::Config) {
            Ok(Some(b)) => b,
            Ok(None) => return Ok(Config::default()),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: display_path,
                    source,
                })
            }
        };

        let src = String::from_utf8(bytes).map_err(|e| ConfigError::Io {
            path: display_path.clone(),
            source: io::Error::new(io::ErrorKind::InvalidData, e),
        })?;

        let raw: serde_json::Value =
            serde_json::from_str(&src).map_err(|source| ConfigError::Parse {
                path: display_path.clone(),
                source,
            })?;

        if let Some(obj) = raw.as_object() {
            let legacy: Vec<String> = LEGACY_REMOVED_KEYS
                .iter()
                .filter(|k| obj.contains_key(**k))
                .map(|k| (*k).to_string())
                .collect();
            if !legacy.is_empty() {
                return Err(ConfigError::LegacyFields {
                    path: display_path,
                    fields: legacy,
                });
            }
        }

        let cfg: Config = serde_json::from_value(raw).map_err(|source| ConfigError::Parse {
            path: display_path.clone(),
            source,
        })?;

        if cfg.schema_version > CURRENT_SCHEMA_VERSION {
            tracing::warn!(
                file = %display_path.display(),
                file_version = cfg.schema_version,
                current_version = CURRENT_SCHEMA_VERSION,
                "config schema_version is newer than this CLI — loading recognised fields only"
            );
        }

        Ok(cfg)
    }

    /// Save configuration atomically into the store.
    pub fn save_config(&self, cfg: &Config) -> Result<(), ConfigError> {
        let display_path = self
            .config_path()
            .map_or_else(|| PathBuf::from("/dev/null"), Path::to_path_buf);

        let mut to_write = cfg.clone();
        if to_write.schema_version == 0 {
            to_write.schema_version = CURRENT_SCHEMA_VERSION;
        }

        let body = serde_json::to_vec_pretty(&to_write).map_err(|source| ConfigError::Parse {
            path: display_path.clone(),
            source,
        })?;

        self.write_asset_atomic(StateAsset::Config, &body)
            .map_err(|source| ConfigError::Io {
                path: display_path,
                source,
            })?;

        Ok(())
    }

    /// Read the persisted webhook secret.
    pub fn read_hook_secret(&self) -> io::Result<Option<String>> {
        self.read_asset_string(StateAsset::HookSecret)
    }

    /// Write the webhook secret atomically.
    pub fn write_hook_secret(&self, secret: &str) -> io::Result<()> {
        self.write_asset_atomic(StateAsset::HookSecret, secret.as_bytes())
    }

    /// Read the pagination cursor if present and non-empty.
    pub fn read_cursor(&self) -> Option<String> {
        match self.read_asset_string(StateAsset::EventsCursor) {
            Ok(Some(raw)) => {
                let trimmed = raw.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            }
            _ => None,
        }
    }

    /// Write pagination cursor. A `None` cursor leaves the existing cursor untouched.
    pub fn write_cursor(&self, next_cursor: Option<&str>) -> io::Result<()> {
        let Some(payload) = next_cursor else {
            return Ok(());
        };
        self.write_asset_atomic(StateAsset::EventsCursor, payload.as_bytes())
    }

    /// Load seen event IDs from `events.seen`.
    pub fn load_seen_lines(&self) -> io::Result<Vec<String>> {
        match self.read_asset_string(StateAsset::EventsSeen)? {
            Some(raw) => Ok(raw
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()),
            None => Ok(Vec::new()),
        }
    }

    /// Append a single seen ID to `events.seen`.
    pub fn append_seen_line(&self, id: &str) -> io::Result<()> {
        match &self.backend {
            StateBackend::Memory { assets } => {
                let mut map = assets.lock().unwrap();
                let entry = map.entry(StateAsset::EventsSeen).or_default();
                entry.extend_from_slice(id.as_bytes());
                entry.push(b'\n');
                Ok(())
            }
            StateBackend::Disk { .. } => {
                self.ensure_dir()?;
                let path = self.path_for(StateAsset::EventsSeen).unwrap();
                #[cfg(unix)]
                let mut f = {
                    use std::os::unix::fs::OpenOptionsExt;
                    fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .mode(0o600)
                        .open(&path)?
                };
                #[cfg(not(unix))]
                let mut f = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?;

                writeln!(f, "{id}")?;
                Ok(())
            }
        }
    }

    /// Rewrite `events.seen` with the given IDs, separated by newlines.
    pub fn rewrite_seen_lines(&self, ids: &std::collections::VecDeque<String>) -> io::Result<()> {
        let mut body = String::new();
        for id in ids {
            body.push_str(id);
            body.push('\n');
        }
        self.write_asset_atomic(StateAsset::EventsSeen, body.as_bytes())
    }
}

/// Drop a `.gitignore` of `*` into the config dir so its plaintext secrets
/// (`api_key`, keystore path, webhook secret) can't be committed by accident.
///
/// `create_new` makes this idempotent and non-destructive: an existing
/// `.gitignore` (user-customized) is left untouched. Best-effort: any other
/// I/O error is logged, not propagated, so it never fails the config write.
///
/// Scoped to the conventional `.taskfast` secrets dir only: a caller who
/// overrides `--config ./config.json` (or `TASKFAST_CONFIG`) must not have a
/// `.gitignore` of `*` dropped into an arbitrary directory.
pub fn ensure_dir_gitignore(dir: &Path) {
    let expected = Path::new(DEFAULT_CONFIG_PATH)
        .parent()
        .and_then(Path::file_name);
    if dir.file_name() != expected {
        return;
    }
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

        store.write_cursor(Some("cur1")).unwrap();
        assert_eq!(store.read_cursor(), Some("cur1".to_string()));

        let cfg = Config {
            api_key: Some("test-key".into()),
            ..Default::default()
        };
        store.save_config(&cfg).unwrap();
        let loaded = store.load_config().unwrap();
        assert_eq!(loaded.api_key, Some("test-key".into()));
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
    fn atomic_write_file_sets_0600_from_creation() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("sub").join("secret.file");
        StateStore::atomic_write_file(&target, b"secret-data").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"secret-data");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "file mode must be 0600");
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
}
