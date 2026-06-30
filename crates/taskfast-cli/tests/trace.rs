// SPDX-License-Identifier: MIT
//! Integration test for the CLI JSONL trace writer (gh#85): `emit` appends one
//! redaction-safe line per call to a per-agent file beside the config.

use serde_json::json;

use taskfast_cli::cmd::CmdResult;
use taskfast_cli::{trace, Envelope, Environment};

#[test]
fn emit_appends_redaction_safe_jsonl_lines() {
    // This is the only test in this binary, so touching process env is safe.
    std::env::remove_var("TASKFAST_TRACE_DIR");
    std::env::remove_var("TASKFAST_TRACE");
    std::env::remove_var("TASKFAST_AGENT");

    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.json");

    // Response data deliberately includes secret-looking keys; only the
    // allowlisted ids may reach disk.
    let ok: CmdResult = Ok(Envelope::success(
        Environment::Staging,
        false,
        json!({ "task_id": "task_xyz", "wallet_password": "leak-me", "api_key": "am_live" }),
    ));
    trace::emit(
        &config_path,
        Some("poster-1"),
        "post",
        &ok,
        Some("req-corr-1"),
    );
    trace::emit(&config_path, Some("poster-1"), "post", &ok, None);

    // Read every file in the dir: the two emits normally share one daily file,
    // but if the test straddles a UTC midnight they split across two. The line
    // count and redaction guarantees hold regardless.
    let mut body = String::new();
    for entry in std::fs::read_dir(tmp.path().join("traces")).expect("traces dir created") {
        body.push_str(&std::fs::read_to_string(entry.unwrap().path()).unwrap());
    }

    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 2, "two emits => two appended lines");

    let rec: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(rec["op"], "post");
    assert_eq!(rec["agent"], "poster-1");
    assert_eq!(rec["kind"], "cli");
    assert_eq!(rec["task_id"], "task_xyz");
    assert_eq!(rec["ok"], true);
    assert_eq!(rec["exit"], 0);
    assert_eq!(
        rec["corr"], "req-corr-1",
        "caller-supplied corr reaches the trace line (gh#91)"
    );

    let rec2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert!(
        rec2.get("corr").is_none(),
        "absent corr is omitted, not null"
    );

    // Redaction: secrets carried in the response data must never be written.
    assert!(!body.contains("wallet_password"), "leaked a flag name");
    assert!(!body.contains("leak-me"), "leaked a secret value");
    assert!(!body.contains("api_key"), "leaked api_key");
    assert!(body.ends_with('\n'), "lines are newline-terminated");
}
