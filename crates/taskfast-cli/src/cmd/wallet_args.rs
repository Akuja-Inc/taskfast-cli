// SPDX-License-Identifier: MIT
//! Shared keystore + password resolution for every command that signs.
//!
//! Each signing subcommand accepts the same triad (`--keystore`,
//! `--wallet-password-file`, `--wallet-address`), threaded through
//! [`crate::tempo_session::WalletOpts`], and resolves the keystore via the same
//! two-step process:
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
use std::sync::atomic::{AtomicBool, Ordering};

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use zeroize::Zeroizing;

use taskfast_agent::keystore::{self, KeySource};

use super::CmdError;

/// One-shot guard so the env-var deprecation warning fires at most once
/// per process. Streaming subcommands could otherwise spam stderr.
static PWD_WARNING_EMITTED: AtomicBool = AtomicBool::new(false);

/// Resolve the clean PathBuf for the keystore, stripping any `file:` prefix.
pub fn resolve_keystore_path(
    cli_keystore: Option<&str>,
    ctx_keystore: Option<&Path>,
) -> Option<PathBuf> {
    let raw = cli_keystore
        .map(str::to_string)
        .or_else(|| ctx_keystore.and_then(|p| p.to_str().map(str::to_string)))?;
    let path_str = raw.strip_prefix("file:").unwrap_or(&raw);
    Some(PathBuf::from(path_str))
}

/// Validate that the decrypted signer address matches an expected address string, if given.
pub fn validate_signer_address(
    signer_address: Address,
    expected_address: Option<&str>,
) -> Result<(), CmdError> {
    if let Some(expected) = expected_address {
        let expected_addr: Address = expected.parse().map_err(|e| {
            CmdError::Usage(format!("--wallet-address is not a valid EVM address: {e}"))
        })?;
        if signer_address != expected_addr {
            return Err(CmdError::Usage(format!(
                "keystore address {:#x} does not match --wallet-address {}",
                signer_address, expected
            )));
        }
    }
    Ok(())
}

/// Decrypt the keystore at `keystore_path` and validate against the expected address.
pub fn load_signer_and_validate(
    keystore_path: Option<&Path>,
    password_file: Option<&Path>,
    expected_address: Option<&str>,
    purpose: &str,
) -> Result<PrivateKeySigner, CmdError> {
    let signer = load_signer(keystore_path, password_file, purpose)?;
    validate_signer_address(signer.address(), expected_address)?;
    Ok(signer)
}

/// Decrypt the keystore at `keystore_path` using the resolved password.
///
/// `purpose` is interpolated into the "missing --keystore" usage error so the
/// operator sees *which* flow demanded a signer (e.g. "submission fee",
/// "settlement approval", "escrow approval").
pub fn load_signer(
    keystore_path: Option<&Path>,
    password_file: Option<&Path>,
    purpose: &str,
) -> Result<PrivateKeySigner, CmdError> {
    let path = keystore_path.ok_or_else(|| {
        CmdError::Usage(format!(
            "--keystore (or TEMPO_KEY_SOURCE) is required to sign the {purpose}"
        ))
    })?;
    let password = resolve_password(password_file)?;
    // `&*password` narrows `Zeroizing<String>` → `&str` only at the
    // keystore boundary; the buffer zeroizes on drop at end of scope.
    keystore::load(
        &KeySource::File {
            path: path.to_path_buf(),
        },
        &password,
    )
    .map_err(CmdError::from)
}

/// Resolve the keystore password with an optional interactive/prompted inline password.
///
/// Precedence:
/// 1. `inline_password` (interactive prompt, avoids being second-guessed by stale env vars)
/// 2. `TASKFAST_WALLET_PASSWORD` environment variable (warns if used)
/// 3. `password_file` contents (single line required, trimmed of trailing CR/LF)
pub fn resolve_password_with_inline(
    inline_password: Option<&str>,
    password_file: Option<&Path>,
) -> Result<Zeroizing<String>, CmdError> {
    resolve_password_with_inline_and_missing_hint(
        inline_password,
        password_file,
        "TASKFAST_WALLET_PASSWORD or --wallet-password-file required to unlock keystore",
    )
}

/// Resolve password with inline precedence, using a custom missing-password message on failure.
pub fn resolve_password_with_inline_and_missing_hint(
    inline_password: Option<&str>,
    password_file: Option<&Path>,
    missing_msg: &str,
) -> Result<Zeroizing<String>, CmdError> {
    if let Some(pw) = inline_password {
        let trimmed = pw.trim_end_matches(['\n', '\r']);
        if !trimmed.is_empty() {
            if trimmed.contains('\n') || trimmed.contains('\r') {
                return Err(CmdError::Usage(
                    "inline wallet password must contain a single line (found an interior newline)"
                        .into(),
                ));
            }
            return Ok(Zeroizing::new(trimmed.to_string()));
        }
    }
    if let Ok(pw) = std::env::var("TASKFAST_WALLET_PASSWORD") {
        if !pw.is_empty() {
            warn_pwd_env_once();
            return Ok(Zeroizing::new(pw));
        }
    }
    let path = password_file.ok_or_else(|| CmdError::Usage(missing_msg.into()))?;
    let raw = Zeroizing::new(std::fs::read_to_string(path).map_err(|e| {
        CmdError::Usage(format!(
            "cannot read wallet password file {}: {e}",
            path.display()
        ))
    })?);
    let trimmed = raw.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return Err(CmdError::Usage(format!(
            "wallet password file {} is empty",
            path.display()
        )));
    }
    // F13: a trailing newline is fine (editors add one), but a newline
    // *inside* the trimmed body almost always means the operator pasted
    // extra content (two passwords, a commented-out line, a stray label).
    // Refuse rather than silently taking the first line as the password.
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(CmdError::Usage(format!(
            "wallet password file {} must contain a single line (found an interior newline)",
            path.display()
        )));
    }
    Ok(Zeroizing::new(trimmed.to_string()))
}

/// Resolve the keystore password: env var wins over file. Trims trailing
/// `\r`/`\n` but rejects a file that is otherwise empty.
pub fn resolve_password(password_file: Option<&Path>) -> Result<Zeroizing<String>, CmdError> {
    resolve_password_with_inline(None, password_file)
}

/// Print the `TASKFAST_WALLET_PASSWORD` deprecation nudge to stderr once
/// per process. Suppressed by `TASKFAST_SUPPRESS_PWD_WARNING=1` so CI
/// pipelines that intentionally use the env var can quiet the noise.
fn warn_pwd_env_once() {
    if std::env::var("TASKFAST_SUPPRESS_PWD_WARNING").is_ok_and(|v| v == "1") {
        return;
    }
    if PWD_WARNING_EMITTED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "warning: TASKFAST_WALLET_PASSWORD is readable via /proc/<pid>/environ; \
         prefer --wallet-password-file (TASKFAST_WALLET_PASSWORD_FILE) or set \
         TASKFAST_SUPPRESS_PWD_WARNING=1 to silence."
    );
}

/// Decode a `0x`-prefixed or bare hex string into raw bytes.
pub fn decode_0x_bytes(s: &str) -> Result<Vec<u8>, String> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    hex::decode(stripped).map_err(|e| e.to_string())
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
        assert_eq!(pw.as_str(), "from-env");
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
    }

    #[test]
    fn password_file_trimmed_when_env_absent() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
        let f = write_temp("secret\r\n");
        let pw = resolve_password(Some(f.path())).expect("ok");
        assert_eq!(pw.as_str(), "secret");
    }

    #[test]
    fn password_rejects_interior_newline() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
        let f = write_temp("first-line\nsecond-line\n");
        let err = resolve_password(Some(f.path())).expect_err("multi-line must fail");
        match err {
            CmdError::Usage(m) => assert!(
                m.contains("single line"),
                "message must name the constraint: {m}"
            ),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn password_rejects_interior_carriage_return() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
        // CRLF in the middle (Windows-edited file with a stray prefix line).
        let f = write_temp("annotation\r\nsecret\n");
        let err = resolve_password(Some(f.path())).expect_err("must fail");
        assert!(matches!(err, CmdError::Usage(_)), "got {err:?}");
    }

    #[test]
    fn password_is_zeroizing_wrapped() {
        // Compile-time proof: return type is Zeroizing<String>, i.e. drop
        // scrubs the backing allocation. Sanity-check via the type name.
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
        let f = write_temp("scrubme\n");
        let pw = resolve_password(Some(f.path())).expect("ok");
        let name = std::any::type_name_of_val(&pw);
        assert!(
            name.contains("Zeroizing"),
            "password must be Zeroizing: {name}"
        );
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
        let err = load_signer(None, None, "escrow approval").expect_err("no keystore → Usage");
        match err {
            CmdError::Usage(m) => {
                assert!(m.contains("escrow approval"), "purpose must appear: {m}")
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn inline_password_wins_and_rejects_interior_newlines() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("TASKFAST_WALLET_PASSWORD", "from-env");
        let pw = resolve_password_with_inline(Some("inline-secret\n"), None).expect("ok");
        assert_eq!(pw.as_str(), "inline-secret");

        let err = resolve_password_with_inline(Some("first\nsecond"), None)
            .expect_err("interior newline must fail");
        assert!(matches!(err, CmdError::Usage(m) if m.contains("single line")));
        std::env::remove_var("TASKFAST_WALLET_PASSWORD");
    }

    #[test]
    fn resolve_keystore_helpers() {
        assert_eq!(
            resolve_keystore_path(Some("file:/tmp/key.json"), None),
            Some(PathBuf::from("/tmp/key.json"))
        );
        assert_eq!(
            resolve_keystore_path(Some("/tmp/key.json"), None),
            Some(PathBuf::from("/tmp/key.json"))
        );
        assert_eq!(
            resolve_keystore_path(None, Some(Path::new("/etc/key.json"))),
            Some(PathBuf::from("/etc/key.json"))
        );
    }

    #[test]
    fn validate_signer_address_checks() {
        let addr = Address::repeat_byte(0x11);
        validate_signer_address(addr, None).expect("no expected address passes");
        validate_signer_address(addr, Some(&format!("{addr:#x}")))
            .expect("matching address passes");

        let err = validate_signer_address(addr, Some("not-hex")).expect_err("invalid address");
        assert!(matches!(err, CmdError::Usage(m) if m.contains("not a valid EVM address")));

        let mismatch = Address::repeat_byte(0x22);
        let err = validate_signer_address(addr, Some(&format!("{mismatch:#x}")))
            .expect_err("mismatch must fail");
        assert!(matches!(err, CmdError::Usage(m) if m.contains("does not match --wallet-address")));
    }
}
