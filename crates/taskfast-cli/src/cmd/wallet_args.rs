//! Shared keystore + password resolution for `post` / `settle` / `escrow`.
//!
//! All three subcommands accept the same triad (`--keystore`,
//! `--wallet-password-file`, `--wallet-address`) and resolve the keystore via
//! the same two-step process:
//!
//!   1. Password: `TASKFAST_WALLET_PASSWORD` env wins (preserves CI); else
//!      read from `--wallet-password-file`, trim trailing newline, reject
//!      empty.
//!   2. Keystore: strip the optional `file:` scheme prefix (`taskfast init`
//!      writes that form to `TEMPO_KEY_SOURCE`), then
//!      `keystore::load(File { path }, password)`.
//!
//! Exposed as free functions (not a flattened clap struct) so each caller
//! keeps its existing `Args` struct — renaming those would churn wiremock
//! tests that import them by name.

use std::path::{Path, PathBuf};

use alloy_signer_local::PrivateKeySigner;

use taskfast_agent::keystore::{self, KeySource};

use super::CmdError;

/// Decrypt the keystore at `keystore_ref` using the resolved password.
///
/// `purpose` is interpolated into the "missing --keystore" usage error so the
/// operator sees *which* flow demanded a signer (e.g. "submission fee",
/// "settlement approval", "escrow approval").
pub fn load_signer(
    keystore_ref: Option<&str>,
    password_file: Option<&Path>,
    purpose: &str,
) -> Result<PrivateKeySigner, CmdError> {
    let raw = keystore_ref.ok_or_else(|| {
        CmdError::Usage(format!(
            "--keystore (or TEMPO_KEY_SOURCE) is required to sign the {purpose}"
        ))
    })?;
    let path_str = raw.strip_prefix("file:").unwrap_or(raw);
    let password = resolve_password(password_file)?;
    let path = PathBuf::from(path_str);
    keystore::load(&KeySource::File { path }, &password).map_err(CmdError::from)
}

/// Resolve the keystore password: env var wins over file. Trims trailing
/// `\r`/`\n` but rejects a file that is otherwise empty.
pub fn resolve_password(password_file: Option<&Path>) -> Result<String, CmdError> {
    if let Ok(pw) = std::env::var("TASKFAST_WALLET_PASSWORD") {
        if !pw.is_empty() {
            return Ok(pw);
        }
    }
    let path = password_file.ok_or_else(|| {
        CmdError::Usage(
            "TASKFAST_WALLET_PASSWORD or --wallet-password-file required to unlock keystore"
                .into(),
        )
    })?;
    let raw = std::fs::read_to_string(path).map_err(|e| {
        CmdError::Usage(format!("read wallet password file {}: {e}", path.display()))
    })?;
    let trimmed = raw.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return Err(CmdError::Usage(format!(
            "wallet password file {} is empty",
            path.display()
        )));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    // Serialize env-var-touching tests — parallel cargo test workers share
    // process env and would clobber each other's assertions otherwise.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn password_env_wins_over_file() {
        let _g = ENV_LOCK.lock().unwrap();
        let f = write_temp("from-file\n");
        std::env::set_var("TASKFAST_WALLET_PASSWORD", "from-env");
        let pw = resolve_password(Some(f.path())).expect("ok");
        assert_eq!(pw, "from-env");
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
    }

    #[test]
    fn password_file_trimmed_when_env_absent() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
        let f = write_temp("secret\r\n");
        let pw = resolve_password(Some(f.path())).expect("ok");
        assert_eq!(pw, "secret");
    }

    #[test]
    fn password_rejects_empty_file() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
        let f = write_temp("\n\n");
        let err = resolve_password(Some(f.path())).expect_err("empty must fail");
        assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
    }

    #[test]
    fn password_requires_file_when_env_absent() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
        let err = resolve_password(None).expect_err("no source → Usage");
        assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
    }

    #[test]
    fn load_signer_missing_keystore_surfaces_purpose() {
        let err = load_signer(None, None, "escrow approval")
            .expect_err("no keystore → Usage");
        match err {
            CmdError::Usage(m) => {
                assert!(m.contains("escrow approval"), "purpose must appear: {m}")
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }
}
