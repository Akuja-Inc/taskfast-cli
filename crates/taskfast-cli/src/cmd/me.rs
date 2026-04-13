//! `taskfast me` — profile + readiness in one envelope.
//!
//! Combines `GET /agents/me` and `GET /agents/me/readiness` so orchestrators
//! can tell in one call whether the agent is authenticated, active, and
//! ready to work. Pure read; honors `--dry-run` by passing through (reads
//! have no side effects to short-circuit).
//!
//! Envelope `data` shape:
//! ```json
//! {
//!   "profile": { ...AgentProfile... },
//!   "readiness": { "ready_to_work": bool, "checks": { ... } },
//!   "ready_to_work": bool
//! }
//! ```
//! The top-level `ready_to_work` is duplicated for grep-friendly orchestrator
//! checks (`jq -e .data.ready_to_work`).

use clap::Parser;
use serde_json::json;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_agent::bootstrap::{get_readiness, validate_auth};

#[derive(Debug, Parser)]
pub struct Args {
    /// Reconstruct in-flight tasks/bids state. Reserved — not yet
    /// implemented; the full resume flow lands with the task+bid list
    /// subcommands (see am-e3u.11).
    #[arg(long)]
    pub resume: bool,
}

pub async fn run(ctx: &Ctx, args: Args) -> CmdResult {
    if args.resume {
        return Err(CmdError::Unimplemented("taskfast me --resume"));
    }

    let client = ctx.client()?;
    let profile = validate_auth(&client).await.map_err(CmdError::from)?;
    let readiness = get_readiness(&client).await.map_err(CmdError::from)?;

    let data = json!({
        "profile": profile,
        "readiness": readiness,
        "ready_to_work": readiness.ready_to_work,
    });
    Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
}
