//! JSON config file for the `taskfast` CLI.
//!
//! Persistent state written by `taskfast init` (and edited via
//! `taskfast config set`) so subsequent subcommands work in a fresh
//! shell without sourcing anything. Replaces the shell-sourceable
//! `.taskfast-agent.env` written by earlier builds — a one-shot
//! migration lives in `Config::load`.
//!
//! # Layout
//!
//! Default path: `./.taskfast/config.json` (project-local). Override via
//! the global `--config <path>` flag or `TASKFAST_CONFIG` env var.
//!
//! # Precedence
//!
//! `Ctx::from_cli_and_config` (see `cmd/mod.rs`) layers values as:
//!
//! ```text
//! clap flag > env var > config file > default
//! ```
//!
//! Clap already folds flag > env via `env = "..."`. The config file sits
//! one rung under that, so omitting a field in JSON never surprises a
//! caller who passed the flag.
//!
//! # Forward-compat
//!
//! `schema_version` is a `u32` that starts at `CURRENT_SCHEMA_VERSION`.
//! Unknown fields are tolerated (default serde behaviour). A file with a
//! newer `schema_version` logs a warning and loads what it recognises —
//! it does not fail, so an older CLI doesn't brick a newer config dir.
//!
//! # Secrets
//!
//! `api_key` lives in this file. The file is written mode `0600` on unix
//! (atomic temp + rename) and the containing `.taskfast/` directory is
//! intended to be git-ignored.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::dotenv::{self, EnvFile};
use crate::Environment;

/// Default project-local path, relative to the CWD.
pub const DEFAULT_CONFIG_PATH: &str = ".taskfast/config.json";

/// Current on-disk schema version. Bump when a field changes shape in a
/// way a reader needs to special-case; additive fields don't need a bump
/// because serde `#[serde(default)]` already handles missing keys.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("I/O on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("legacy dotenv {path}: {source}")]
    LegacyDotenv {
        path: PathBuf,
        #[source]
        source: dotenv::DotenvError,
    },
}

/// On-disk config. Every runtime field is `Option` so the file stays
/// small and a partially-configured project can still round-trip.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    /// On-disk schema version (`CURRENT_SCHEMA_VERSION` for fresh
    /// writes). A `0` value means "not set" and is normalized to
    /// `CURRENT_SCHEMA_VERSION` on save.
    #[serde(skip_serializing_if = "is_zero")]
    pub schema_version: u32,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<Environment>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,

    /// Agent API key. Secret — the file is written `0600`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    /// `mainnet` or `testnet`. Kept as a string rather than a typed enum
    /// so new Tempo networks don't force a schema bump.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet_address: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub keystore_path: Option<PathBuf>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_secret_path: Option<PathBuf>,
}

// `skip_serializing_if` is required by serde to take `&T`, so clippy's
// pass-by-value lint is a false positive here.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(v: &u32) -> bool {
    *v == 0
}

impl Config {
    /// Parse the JSON at `path`. Missing file → `Config::default()`
    /// (callers treat absence the same as an empty config). Newer
    /// `schema_version` values log a warning via `tracing::warn!` and
    /// load what they recognise.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let src = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let cfg: Config = serde_json::from_str(&src).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if cfg.schema_version > CURRENT_SCHEMA_VERSION {
            tracing::warn!(
                file = %path.display(),
                file_version = cfg.schema_version,
                current_version = CURRENT_SCHEMA_VERSION,
                "config schema_version is newer than this CLI — loading recognised fields only"
            );
        }
        Ok(cfg)
    }

    /// Atomic(ish) write: JSON-serialize to a sibling `.tmp`, chmod
    /// `0600` on unix, rename into place. Creates the parent directory
    /// if it doesn't exist.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
        }
        let mut to_write = self.clone();
        if to_write.schema_version == 0 {
            to_write.schema_version = CURRENT_SCHEMA_VERSION;
        }
        let body = serde_json::to_vec_pretty(&to_write).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

        let tmp = path.with_extension("json.tmp");
        {
            let mut f = fs::File::create(&tmp).map_err(|source| ConfigError::Io {
                path: tmp.clone(),
                source,
            })?;
            f.write_all(&body).map_err(|source| ConfigError::Io {
                path: tmp.clone(),
                source,
            })?;
            f.write_all(b"\n").map_err(|source| ConfigError::Io {
                path: tmp.clone(),
                source,
            })?;
            f.flush().map_err(|source| ConfigError::Io {
                path: tmp.clone(),
                source,
            })?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(&tmp, perms).map_err(|source| ConfigError::Io {
                path: tmp.clone(),
                source,
            })?;
        }
        fs::rename(&tmp, path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(())
    }

    /// Default file path, rooted in the caller's current working
    /// directory. Used when neither `--config` nor `TASKFAST_CONFIG`
    /// is supplied.
    pub fn default_path() -> PathBuf {
        PathBuf::from(DEFAULT_CONFIG_PATH)
    }

    /// Like [`Config::load`], plus a one-shot migration: if the JSON
    /// file is missing and a legacy `.taskfast-agent.env` exists in
    /// the expected project root (the grandparent of `path`, i.e. the
    /// directory above `.taskfast/`), its contents are folded into a
    /// fresh `Config`, the JSON is written, and a one-line note is
    /// emitted via `tracing::info!`.
    ///
    /// Subsequent runs find the JSON and skip this path entirely. The
    /// old dotenv file is left on disk — the user removes it when
    /// comfortable that the migration took.
    ///
    /// Pure JSON load (no migration) is still available via
    /// [`Config::load`].
    pub fn load_or_migrate(path: &Path) -> Result<Self, ConfigError> {
        if path.exists() {
            Self::load(path)
        } else {
            Self::try_migrate(path, legacy_dotenv_for(path).as_deref())
        }
    }

    fn try_migrate(
        config_path: &Path,
        legacy_dotenv: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        let Some(legacy) = legacy_dotenv else {
            return Ok(Self::default());
        };
        if !legacy.exists() {
            return Ok(Self::default());
        }
        let env = EnvFile::load(legacy).map_err(|source| ConfigError::LegacyDotenv {
            path: legacy.to_path_buf(),
            source,
        })?;
        let cfg = Self::from_legacy_dotenv(&env);
        cfg.save(config_path)?;
        tracing::info!(
            legacy = %legacy.display(),
            config = %config_path.display(),
            "migrated .taskfast-agent.env into JSON config — legacy file left in place",
        );
        Ok(cfg)
    }

    /// Map an in-memory legacy dotenv into a `Config`. Exposed for the
    /// `config` subcommand's `--migrate` flag and for tests.
    pub fn from_legacy_dotenv(env: &EnvFile) -> Self {
        let keystore_path = env
            .get("TEMPO_KEY_SOURCE")
            .and_then(|s| s.strip_prefix("file:").map(PathBuf::from));
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            environment: None,
            api_base: env.get("TASKFAST_API").map(str::to_string),
            api_key: env.get("TASKFAST_API_KEY").map(str::to_string),
            network: env.get("TEMPO_NETWORK").map(str::to_string),
            wallet_address: env.get("TEMPO_WALLET_ADDRESS").map(str::to_string),
            keystore_path,
            agent_id: None,
            webhook_url: None,
            webhook_secret_path: None,
        }
    }
}

/// Resolve the legacy dotenv path for a given config path. For the
/// default `./.taskfast/config.json` this returns `./.taskfast-agent.env`
/// — the project-root sibling of the `.taskfast/` directory. Returns
/// `None` when `path` has no grandparent (flat paths like
/// `"config.json"` don't get a migration target).
fn legacy_dotenv_for(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    let grandparent = parent.parent()?;
    Some(grandparent.join(dotenv::DEFAULT_ENV_FILENAME))
}

// Serde support for `Environment` — declared here (not in lib.rs) so the
// config module owns its serialization contract. If the enum ever grows
// a runtime variant that shouldn't be persisted, the mapping stays
// local.
impl Serialize for Environment {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(match self {
            Self::Prod => "prod",
            Self::Staging => "staging",
            Self::Local => "local",
        })
    }
}

impl<'de> Deserialize<'de> for Environment {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        match s.as_str() {
            "prod" | "production" => Ok(Self::Prod),
            "staging" => Ok(Self::Staging),
            "local" => Ok(Self::Local),
            other => Err(serde::de::Error::custom(format!(
                "unknown environment {other:?}; expected prod | staging | local"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample() -> Config {
        Config {
            schema_version: CURRENT_SCHEMA_VERSION,
            environment: Some(Environment::Local),
            api_base: Some("http://localhost:4000".into()),
            api_key: Some("am_live_secret".into()),
            network: Some("testnet".into()),
            wallet_address: Some("0xabc".into()),
            keystore_path: Some(PathBuf::from("/tmp/keystore.json")),
            agent_id: Some("agent_123".into()),
            webhook_url: Some("https://example.com/hook".into()),
            webhook_secret_path: Some(PathBuf::from("/tmp/hook.secret")),
        }
    }

    #[test]
    fn load_missing_file_returns_default() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope.json");
        let cfg = Config::load(&missing).expect("missing file is default, not error");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn roundtrip_preserves_every_field() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sub").join("config.json");
        let original = sample();
        original.save(&path).expect("save");
        let loaded = Config::load(&path).expect("load");
        assert_eq!(loaded, original);
    }

    #[test]
    fn save_zero_schema_version_is_normalized_to_current() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        let cfg = Config {
            api_base: Some("http://x".into()),
            ..Config::default()
        };
        assert_eq!(cfg.schema_version, 0);
        cfg.save(&path).expect("save");
        let loaded = Config::load(&path).expect("load");
        assert_eq!(loaded.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn save_creates_missing_parent_dir() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b").join("config.json");
        sample().save(&nested).expect("save into nested path");
        assert!(nested.exists());
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        let body = r#"{
            "schema_version": 1,
            "api_base": "http://x",
            "future_field": "ignored",
            "another": {"nested": true}
        }"#;
        fs::write(&path, body).unwrap();
        let cfg = Config::load(&path).expect("unknown fields should not fail load");
        assert_eq!(cfg.api_base.as_deref(), Some("http://x"));
    }

    #[test]
    fn newer_schema_version_loads_with_warning() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        let body = format!(
            r#"{{ "schema_version": {}, "api_base": "http://x" }}"#,
            CURRENT_SCHEMA_VERSION + 5
        );
        fs::write(&path, body).unwrap();
        let cfg = Config::load(&path).expect("newer version still loads");
        assert_eq!(cfg.schema_version, CURRENT_SCHEMA_VERSION + 5);
        assert_eq!(cfg.api_base.as_deref(), Some("http://x"));
    }

    #[test]
    fn empty_config_serializes_compactly() {
        // A default config has no runtime fields set — the JSON should
        // be close to `{}` (just an empty object after skip_if).
        let body = serde_json::to_string(&Config::default()).unwrap();
        assert_eq!(body, "{}", "default config should serialize to {{}}");
    }

    #[test]
    fn environment_roundtrip() {
        for env in [Environment::Prod, Environment::Staging, Environment::Local] {
            let cfg = Config {
                environment: Some(env),
                ..Config::default()
            };
            let body = serde_json::to_string(&cfg).unwrap();
            let back: Config = serde_json::from_str(&body).unwrap();
            assert_eq!(back.environment.map(|e| e.as_str()), Some(env.as_str()));
        }
    }

    #[test]
    fn environment_accepts_production_alias() {
        let body = r#"{ "environment": "production" }"#;
        let cfg: Config = serde_json::from_str(body).unwrap();
        assert!(matches!(cfg.environment, Some(Environment::Prod)));
    }

    #[test]
    fn environment_rejects_unknown() {
        let body = r#"{ "environment": "moon" }"#;
        let err = serde_json::from_str::<Config>(body).unwrap_err();
        assert!(err.to_string().contains("moon"));
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_mode_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        sample().save(&path).expect("save");
        let meta = fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected mode 0600, got {mode:o}");
    }

    #[test]
    fn parse_error_includes_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.json");
        fs::write(&path, "{ not json").unwrap();
        let err = Config::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bad.json"), "error mentions path: {msg}");
    }

    #[test]
    fn legacy_dotenv_for_default_layout() {
        let p = PathBuf::from(".taskfast/config.json");
        assert_eq!(
            legacy_dotenv_for(&p),
            Some(PathBuf::from(".taskfast-agent.env")),
        );
    }

    #[test]
    fn legacy_dotenv_for_absolute_layout() {
        let p = PathBuf::from("/proj/.taskfast/config.json");
        assert_eq!(
            legacy_dotenv_for(&p),
            Some(PathBuf::from("/proj/.taskfast-agent.env")),
        );
    }

    #[test]
    fn legacy_dotenv_for_flat_path_is_none() {
        // "config.json" has no parent.parent() — nothing to migrate from.
        let p = PathBuf::from("config.json");
        assert_eq!(legacy_dotenv_for(&p), None);
    }

    #[test]
    fn from_legacy_dotenv_maps_every_known_key() {
        let mut env = EnvFile::new();
        env.set("TASKFAST_API", "http://x");
        env.set("TASKFAST_API_KEY", "am_live_abc");
        env.set("TEMPO_NETWORK", "testnet");
        env.set("TEMPO_WALLET_ADDRESS", "0xdead");
        env.set("TEMPO_KEY_SOURCE", "file:/tmp/keys.json");
        let cfg = Config::from_legacy_dotenv(&env);
        assert_eq!(cfg.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(cfg.api_base.as_deref(), Some("http://x"));
        assert_eq!(cfg.api_key.as_deref(), Some("am_live_abc"));
        assert_eq!(cfg.network.as_deref(), Some("testnet"));
        assert_eq!(cfg.wallet_address.as_deref(), Some("0xdead"));
        assert_eq!(cfg.keystore_path, Some(PathBuf::from("/tmp/keys.json")));
        assert!(cfg.environment.is_none());
        assert!(cfg.agent_id.is_none());
    }

    #[test]
    fn from_legacy_dotenv_without_file_prefix_drops_keystore_path() {
        // Defensive: old init.sh versions could have written a bare path.
        // We only migrate values we understand.
        let mut env = EnvFile::new();
        env.set("TEMPO_KEY_SOURCE", "/tmp/keys.json");
        let cfg = Config::from_legacy_dotenv(&env);
        assert!(cfg.keystore_path.is_none());
    }

    #[test]
    fn load_or_migrate_reads_dotenv_when_json_missing() {
        let tmp = TempDir::new().unwrap();
        let dotenv_path = tmp.path().join(".taskfast-agent.env");
        let config_path = tmp.path().join(".taskfast").join("config.json");

        let mut env = EnvFile::new();
        env.set("TASKFAST_API", "http://migrated");
        env.set("TASKFAST_API_KEY", "am_live_migrated");
        env.set("TEMPO_NETWORK", "testnet");
        env.save(&dotenv_path).unwrap();

        let cfg = Config::load_or_migrate(&config_path).expect("migration runs");
        assert_eq!(cfg.api_base.as_deref(), Some("http://migrated"));
        assert_eq!(cfg.api_key.as_deref(), Some("am_live_migrated"));
        assert_eq!(cfg.network.as_deref(), Some("testnet"));

        // Side effect: JSON now exists on disk so the next call skips
        // the migration path.
        assert!(config_path.exists(), "migration writes JSON");
        assert!(dotenv_path.exists(), "legacy dotenv is left untouched");
    }

    #[test]
    fn load_or_migrate_ignores_dotenv_when_json_exists() {
        let tmp = TempDir::new().unwrap();
        let dotenv_path = tmp.path().join(".taskfast-agent.env");
        let config_path = tmp.path().join(".taskfast").join("config.json");

        // Seed both — JSON should win.
        let mut env = EnvFile::new();
        env.set("TASKFAST_API", "http://dotenv-wins");
        env.save(&dotenv_path).unwrap();

        let json_cfg = Config {
            api_base: Some("http://json-wins".into()),
            ..Config::default()
        };
        json_cfg.save(&config_path).unwrap();

        let cfg = Config::load_or_migrate(&config_path).expect("json wins");
        assert_eq!(cfg.api_base.as_deref(), Some("http://json-wins"));
    }

    #[test]
    fn load_or_migrate_returns_default_when_nothing_exists() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join(".taskfast").join("config.json");
        let cfg = Config::load_or_migrate(&config_path).expect("no files, no error");
        assert_eq!(cfg, Config::default());
        assert!(!config_path.exists(), "nothing to migrate, nothing to write");
    }
}
