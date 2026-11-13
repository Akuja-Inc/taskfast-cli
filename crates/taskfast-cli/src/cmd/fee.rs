// SPDX-License-Identifier: MIT
//! `taskfast fee retry` — re-attempt a failed submission-fee charge.
//!
//! Wraps `POST /tasks/{id}/retry-fee`. Since 2026-11-01 the server requires
//! the poster's freshly signed fee transfer (a 0x-hex signed ERC-20 transfer,
//! or the hash of one the poster already broadcast) in the body; the original
//! voucher cannot be reused. The CLI passes `--submission-fee-voucher` through
//! unchanged: it does not sign, send, or look up a fee transfer, and it does
//! not validate the hex (the server answers 400 `invalid_submission_fee_voucher`).
//! 409 `retry_not_needed` / `retry_in_progress` surface with their stable codes.

use clap::Parser;
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::api::types::RetrySubmissionFeeRequest;
use taskfast_client::map_api_error;

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Poster: re-attempt the submission-fee charge with a freshly signed fee transfer.
    Retry(RetryArgs),
}

#[derive(Debug, Parser)]
pub struct RetryArgs {
    /// Task UUID (parked at `blocked_on_submission_fee_debt`).
    pub task_id: String,

    /// The poster's freshly signed submission-fee transfer (0x-hex), or the
    /// hash of one already broadcast. The original voucher cannot be reused.
    #[arg(long)]
    pub submission_fee_voucher: String,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Retry(a) => retry(ctx, a).await,
    }
}

async fn retry(ctx: &Ctx, args: RetryArgs) -> CmdResult {
    let task_id = Uuid::parse_str(&args.task_id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    if args.submission_fee_voucher.trim().is_empty() {
        return Err(CmdError::Usage(
            "--submission-fee-voucher is empty; the server requires the poster's freshly signed \
             fee transfer (or the hash of one the poster broadcast). The original voucher \
             cannot be reused."
                .into(),
        ));
    }
    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_retry_fee",
                "task_id": task_id.to_string(),
                "submission_fee_voucher": args.submission_fee_voucher,
            }),
        ));
    }
    let client = ctx.client()?;
    // The generated request type carries the spec's own `^0x(hex pairs)+$`
    // pattern, so it cannot hold a non-hex string; we do not hand-copy the rule.
    let body = RetrySubmissionFeeRequest {
        submission_fee_voucher: args.submission_fee_voucher.parse().map_err(|e| {
            CmdError::Usage(format!(
                "--submission-fee-voucher is not 0x-prefixed hex (the server would answer \
                 400 invalid_submission_fee_voucher): {e}"
            ))
        })?,
    };
    let resp = match client.inner().retry_submission_fee(&task_id, &body).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": resp.task_id,
            "status": resp.status,
            "message": resp.message,
        }),
    ))
}
