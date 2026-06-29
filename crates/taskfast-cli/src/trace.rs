// SPDX-License-Identifier: MIT
//! CLI-native JSONL command trace (gh#85).
//!
//! Appends one JSON line per invocation to a per-agent, per-day file under
//! `traces/` (next to `config.json`, override with `TASKFAST_TRACE_DIR`). The
//! line's `corr` is the server `x-request-id`, the join key back to
//! `mix taskfast.trace <task_id>`.
//!
//! **Redaction is the whole point.** The CLI holds keystores, wallet passwords
//! and signed payloads, so the record is a strict *allowlist*: subcommand path
//! (never flag values), the server correlation id, response-derived ids, the
//! exit class, and a stable error *category* (never a free-form message). A new
//! field is opt-in by construction — nothing reaches disk unless added here.
//!
//! Writing is best-effort: any failure is logged at debug and swallowed so the
//! trace can never change a command's outcome.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cmd::CmdResult;

const TRACE_DIR_ENV: &str = "TASKFAST_TRACE_DIR";
const TRACE_TOGGLE_ENV: &str = "TASKFAST_TRACE";
const AGENT_ENV: &str = "TASKFAST_AGENT";

/// One redaction-safe trace line. Field order mirrors the AgentTrace
/// vocabulary so server- and CLI-emitted lines align.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TraceRecord {
    /// RFC3339 with millis, e.g. `2026-06-29T00:00:00.000Z`.
    pub at: String,
    pub agent: String,
    /// Subcommand path only (e.g. `escrow sign`). Never argument values.
    pub op: String,
    /// Always `cli` for CLI-native lines (`http` is the server's vocabulary).
    pub kind: &'static str,
    /// Server `x-request-id`; absent on purely local commands / transport fail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bid_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escrow_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    pub exit: i32,
    pub ok: bool,
    /// Stable error category from `CmdError::code()` (a fixed set of static
    /// strings). Never a message — messages may echo user input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'static str>,
}

/// Walk clap's matched subcommand names into a space-joined path. This reads
/// only subcommand *names*, never argument values — that is the redaction
/// guarantee for `op`: `escrow sign --wallet-password X` yields `escrow sign`.
pub fn subcommand_path(matches: &clap::ArgMatches) -> String {
    let mut parts = Vec::new();
    let mut cur = matches;
    while let Some((name, sub)) = cur.subcommand() {
        parts.push(name);
        cur = sub;
    }
    parts.join(" ")
}

/// Whether tracing is on. On by default; `--no-trace` or `TASKFAST_TRACE` in
/// {0,false,off,no} turns it off.
pub fn enabled(no_trace_flag: bool) -> bool {
    !no_trace_flag && toggle_from_env(std::env::var(TRACE_TOGGLE_ENV).ok().as_deref())
}

fn toggle_from_env(value: Option<&str>) -> bool {
    match value {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        None => true,
    }
}

/// Emit a trace line for one invocation. Best-effort: never propagates errors.
pub fn emit(config_path: &Path, agent_id: Option<&str>, op: &str, result: &CmdResult) {
    if let Err(e) = try_emit(config_path, agent_id, op, result) {
        tracing::debug!(error = %e, "cli trace write failed (ignored)");
    }
}

fn try_emit(
    config_path: &Path,
    agent_id: Option<&str>,
    op: &str,
    result: &CmdResult,
) -> std::io::Result<()> {
    let now = chrono::Utc::now();
    let agent = resolve_agent(std::env::var(AGENT_ENV).ok().as_deref(), agent_id);
    let record = build_record(
        now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        agent.clone(),
        op,
        taskfast_client::take_last_corr(),
        result,
    );

    let mut line = serde_json::to_string(&record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');

    let dir = resolve_dir(config_path, std::env::var(TRACE_DIR_ENV).ok().as_deref());
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!(
        "trace-{}-{}.jsonl",
        sanitize(&agent),
        now.format("%Y-%m-%d")
    ));
    // O_APPEND + a single sub-PIPE_BUF write is atomic across processes on
    // POSIX, so concurrent agents never interleave lines (~200 B << 4096).
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?
        .write_all(line.as_bytes())
}

fn build_record(
    at: String,
    agent: String,
    op: &str,
    corr: Option<String>,
    result: &CmdResult,
) -> TraceRecord {
    let (exit, ok, error, data) = match result {
        Ok(env) => (0, true, None, env.data.as_ref()),
        Err(e) => (e.exit_code() as i32, false, Some(e.code()), None),
    };
    TraceRecord {
        at,
        agent,
        op: op.to_string(),
        kind: "cli",
        corr,
        task_id: id_from(data, "task_id"),
        bid_id: id_from(data, "bid_id"),
        escrow_id: id_from(data, "escrow_id"),
        tx_hash: tx_hash_from(data),
        exit,
        ok,
        error,
    }
}

/// Pull a top-level string id out of a success envelope's `data`. Only the
/// allowlisted keys are ever read — arbitrary response fields never leak.
fn id_from(data: Option<&serde_json::Value>, key: &str) -> Option<String> {
    data?.get(key)?.as_str().map(str::to_string)
}

/// Pull the transaction hash, which commands surface under varied keys
/// (`tx_hash`, `submission_fee_tx_hash`, `approval_tx_hash`, ...). A tx hash is
/// a public on-chain value — never secret — and a response carries one primary
/// hash, so taking any top-level `*tx_hash` key is safe and unambiguous.
fn tx_hash_from(data: Option<&serde_json::Value>) -> Option<String> {
    let obj = data?.as_object()?;
    if let Some(v) = obj.get("tx_hash").and_then(serde_json::Value::as_str) {
        return Some(v.to_string());
    }
    obj.iter()
        .find(|(k, _)| k.ends_with("tx_hash"))
        .and_then(|(_, v)| v.as_str())
        .map(str::to_string)
}

fn resolve_dir(config_path: &Path, env_override: Option<&str>) -> PathBuf {
    if let Some(dir) = env_override.filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("traces")
}

fn resolve_agent(env_agent: Option<&str>, agent_id: Option<&str>) -> String {
    env_agent
        .filter(|s| !s.is_empty())
        .or(agent_id.filter(|s| !s.is_empty()))
        .unwrap_or("unknown")
        .to_string()
}

/// Filesystem-safe label for the trace filename (the `agent` field stays raw).
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::CmdError;
    use crate::envelope::Envelope;
    use crate::Environment;
    use serde_json::json;

    fn ok_envelope(data: serde_json::Value) -> Envelope {
        Envelope::success(Environment::Staging, false, data)
    }

    #[test]
    fn tx_hash_matches_prefixed_keys() {
        // Commands surface the hash under varied keys; all must populate it.
        let data = json!({ "submission_fee_tx_hash": "0xfee" });
        assert_eq!(tx_hash_from(Some(&data)).as_deref(), Some("0xfee"));
        // The canonical key wins when both are present.
        let data = json!({ "tx_hash": "0xprimary", "approval_tx_hash": "0xappr" });
        assert_eq!(tx_hash_from(Some(&data)).as_deref(), Some("0xprimary"));
        assert_eq!(tx_hash_from(Some(&json!({ "other": 1 }))), None);
    }

    #[test]
    fn op_is_subcommand_path_without_flag_values() {
        let cmd = clap::Command::new("taskfast").subcommand(
            clap::Command::new("escrow").subcommand(
                clap::Command::new("sign")
                    .arg(clap::Arg::new("wallet-password").long("wallet-password")),
            ),
        );
        let matches =
            cmd.get_matches_from(["taskfast", "escrow", "sign", "--wallet-password", "hunter2"]);
        let op = subcommand_path(&matches);
        assert_eq!(op, "escrow sign");
        // Redaction: the secret value must not appear anywhere in `op`.
        assert!(!op.contains("hunter2"));
    }

    #[test]
    fn success_record_pulls_allowlisted_ids_only() {
        let record = build_record(
            "2026-06-29T00:00:00.000Z".into(),
            "poster-1".into(),
            "post",
            Some("req-9".into()),
            &Ok(ok_envelope(json!({
                "task_id": "task_abc",
                "tx_hash": "0xdead",
                "wallet_password": "should-never-appear",
                "api_key": "am_live_secret",
            }))),
        );
        assert_eq!(record.task_id.as_deref(), Some("task_abc"));
        assert_eq!(record.tx_hash.as_deref(), Some("0xdead"));
        assert_eq!(record.corr.as_deref(), Some("req-9"));
        assert!(record.ok);
        assert_eq!(record.exit, 0);
        assert_eq!(record.kind, "cli");

        // The serialized line must carry no non-allowlisted field, especially
        // not secret-looking ones present in the response data.
        let line = serde_json::to_string(&record).unwrap();
        assert!(!line.contains("wallet_password"));
        assert!(!line.contains("api_key"));
        assert!(!line.contains("am_live_secret"));
    }

    #[test]
    fn error_record_uses_stable_category_not_message() {
        let err = CmdError::Auth("token rejected for user@example.com".into());
        let code = err.code();
        let exit = err.exit_code() as i32;
        let record = build_record(
            "2026-06-29T00:00:00.000Z".into(),
            "poster-1".into(),
            "me",
            None,
            &Err(err),
        );
        assert!(!record.ok);
        assert_eq!(record.exit, exit);
        assert_eq!(record.error, Some(code));
        // The free-form message (which could echo user input) must not leak.
        let line = serde_json::to_string(&record).unwrap();
        assert!(!line.contains("user@example.com"));
    }

    #[test]
    fn toggle_defaults_on_and_respects_opt_out() {
        assert!(toggle_from_env(None));
        assert!(toggle_from_env(Some("1")));
        assert!(toggle_from_env(Some("yes")));
        for off in ["0", "false", "off", "no", "OFF", " false "] {
            assert!(!toggle_from_env(Some(off)), "{off} should disable");
        }
        assert!(!enabled(true)); // --no-trace wins regardless of env
    }

    #[test]
    fn dir_and_agent_resolution() {
        let cfg = Path::new(".taskfast/config.json");
        assert_eq!(resolve_dir(cfg, None), PathBuf::from(".taskfast/traces"));
        assert_eq!(
            resolve_dir(cfg, Some("/custom/dir")),
            PathBuf::from("/custom/dir")
        );
        assert_eq!(resolve_agent(Some("env-agent"), Some("cfg")), "env-agent");
        assert_eq!(resolve_agent(None, Some("cfg-agent")), "cfg-agent");
        assert_eq!(resolve_agent(None, None), "unknown");
        assert_eq!(sanitize("agent/1:2"), "agent-1-2");
    }
}
