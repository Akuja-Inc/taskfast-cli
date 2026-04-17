//! `taskfast dispute` — fetch the dispute record on a task.
//!
//! Thin wrapper over `GET /tasks/{id}/dispute`. Useful for reading the
//! current dispute state (who opened it, the claim text, remedy deadline,
//! and any arbitration notes) without shelling into the event stream.

use clap::Parser;
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::map_api_error;

#[derive(Debug, Parser)]
pub struct Args {
    pub task_id: String,
}

pub async fn run(ctx: &Ctx, args: Args) -> CmdResult {
    let task_id = Uuid::parse_str(&args.task_id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    let client = ctx.client()?;
    let resp = match client.inner().get_task_dispute(&task_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": task_id.to_string(),
            "dispute": resp,
        }),
    ))
}
