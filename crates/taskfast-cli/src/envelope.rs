//! JSON output envelope — uniform across success/error/dry-run.

use serde::Serialize;

use crate::Environment;
use crate::cmd::CmdError;

#[derive(Debug, Serialize)]
pub struct Envelope {
    pub ok: bool,
    pub environment: &'static str,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

#[derive(Debug, Serialize)]
pub struct ErrorPayload {
    pub code: &'static str,
    pub message: String,
}

impl Envelope {
    pub fn success(env: Environment, dry_run: bool, data: serde_json::Value) -> Self {
        Self { ok: true, environment: env.as_str(), dry_run, data: Some(data), error: None }
    }

    pub fn error(env: Environment, dry_run: bool, err: &CmdError) -> Self {
        Self {
            ok: false,
            environment: env.as_str(),
            dry_run,
            data: None,
            error: Some(ErrorPayload { code: err.code(), message: err.to_string() }),
        }
    }

    pub fn emit(&self) {
        // Flush is implicit — stdout closes on process exit.
        let _ = serde_json::to_writer(std::io::stdout().lock(), self);
        println!();
    }
}
