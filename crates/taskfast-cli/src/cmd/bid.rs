//! `taskfast bid` — read + mutate operations on bids.
//!
//! This slice (am-4yr) implements the **read path** only: `list`.
//! Mutations (`create`, `cancel`, `accept`, `reject`) stay as
//! `Unimplemented` stubs so `main.rs` dispatch keeps compiling; each one
//! lands in its own bead once signing/escrow semantics are nailed down.
//!
//! `GET /agents/me/bids` is pure read, cursor-paginated via `PaginatedMeta`,
//! and mirrors the `task list` envelope shape (`{bids, meta}`).

use clap::{Parser, Subcommand};
use serde_json::json;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::TaskFastClient;
use taskfast_client::map_api_error;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// GET /agents/me/bids — bids placed by this agent.
    List(ListArgs),
    /// Worker: place a bid. (Deferred — needs bid payload shape confirmed.)
    Create {
        task_id: String,
        #[arg(long)]
        amount: String,
    },
    /// Worker: withdraw an open bid. (Deferred.)
    Cancel { id: String },
    /// Poster: accept a bid. (Deferred — escrow delegation; see am-4w2.)
    Accept { id: String },
    /// Poster: reject a bid. (Deferred.)
    Reject { id: String },
}

#[derive(Debug, Parser)]
pub struct ListArgs {
    /// Opaque pagination cursor from a previous response's `next_cursor`.
    #[arg(long)]
    pub cursor: Option<String>,

    /// Max items per page. Server enforces its own ceiling; we pass through.
    #[arg(long)]
    pub limit: Option<i64>,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::List(args) => list(ctx, args).await,
        Command::Create { .. } => Err(CmdError::Unimplemented("taskfast bid create")),
        Command::Cancel { .. } => Err(CmdError::Unimplemented("taskfast bid cancel")),
        Command::Accept { .. } => Err(CmdError::Unimplemented("taskfast bid accept")),
        Command::Reject { .. } => Err(CmdError::Unimplemented("taskfast bid reject")),
    }
}

async fn list(ctx: &Ctx, args: ListArgs) -> CmdResult {
    let client = ctx.client()?;
    let data = list_bids(&client, &args).await?;
    Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
}

async fn list_bids(
    client: &TaskFastClient,
    args: &ListArgs,
) -> Result<serde_json::Value, CmdError> {
    let resp = match client
        .inner()
        .get_agent_bids(args.cursor.as_deref(), args.limit)
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(json!({
        "bids": resp.data,
        "meta": resp.meta,
    }))
}
