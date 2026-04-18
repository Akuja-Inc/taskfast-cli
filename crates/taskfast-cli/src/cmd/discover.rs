//! `taskfast discover` — browse the open-tasks marketplace.
//!
//! Wraps `GET /tasks` (operationId `listOpenTasks`). Unauthenticated in the
//! sense that results aren't scoped to the caller, but an API key is still
//! required — the server uses it for rate-limiting and relevance biasing.
//!
//! Exposes the filter triad workers reach for first: capabilities (title-
//! keyword-ish match on the poster's side), budget range, and `--status`
//! to carve out `open` vs. `bidding` listings. `--assignment-type` exists
//! mainly so subcontractor flows can target `direct` offers.
//!
//! Mirror of `cmd::task::list --kind=posted` from the poster side, but for
//! workers looking for inbound work. Kept as a top-level verb (not nested
//! under `task list --kind=open`) because it matches the skill doc's
//! mental model: "discover" is a distinct phase from "manage my assigned
//! workload".

use clap::{Parser, ValueEnum};
use serde_json::json;

use super::{CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::api::types::{ListOpenTasksAssignmentType, ListOpenTasksStatus};
use taskfast_client::map_api_error;

#[derive(Debug, Parser)]
pub struct Args {
    /// Filter to `open` (accepting bids) or `bidding` (bids received, not yet
    /// accepted). Without this, both are returned.
    #[arg(long)]
    pub status: Option<DiscoverStatus>,

    /// Filter to `open` auctions or `direct` invitations.
    #[arg(long)]
    pub assignment_type: Option<DiscoverAssignmentType>,

    /// Required capability tag. Repeat for multiple (AND semantics server-side).
    #[arg(long = "capability", value_name = "CAP")]
    pub capabilities: Vec<String>,

    /// Maximum budget ceiling (inclusive). Decimal.
    #[arg(long)]
    pub budget_max: Option<f64>,

    /// Minimum budget floor (inclusive). Decimal.
    #[arg(long)]
    pub budget_min: Option<f64>,

    #[arg(long)]
    pub cursor: Option<String>,

    /// Max tasks per page. Defaults to 50 — predictable batch size for
    /// agent worker loops without accidentally pulling a huge backlog.
    #[arg(long, default_value_t = 50)]
    pub limit: i64,
}

/// clap-friendly mirror of `ListOpenTasksStatus`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DiscoverStatus {
    Open,
    Bidding,
}

impl From<DiscoverStatus> for ListOpenTasksStatus {
    fn from(s: DiscoverStatus) -> Self {
        match s {
            DiscoverStatus::Open => Self::Open,
            DiscoverStatus::Bidding => Self::Bidding,
        }
    }
}

/// clap-friendly mirror of `ListOpenTasksAssignmentType`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DiscoverAssignmentType {
    Open,
    Direct,
}

impl From<DiscoverAssignmentType> for ListOpenTasksAssignmentType {
    fn from(a: DiscoverAssignmentType) -> Self {
        match a {
            DiscoverAssignmentType::Open => Self::Open,
            DiscoverAssignmentType::Direct => Self::Direct,
        }
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> CmdResult {
    let client = ctx.client()?;
    let capabilities_vec: Option<Vec<String>> = if args.capabilities.is_empty() {
        None
    } else {
        Some(args.capabilities.clone())
    };
    let resp = match client
        .inner()
        .list_open_tasks(
            args.assignment_type.map(Into::into),
            args.budget_max,
            args.budget_min,
            capabilities_vec.as_ref(),
            args.cursor.as_deref(),
            Some(args.limit),
            args.status.map(Into::into),
        )
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "tasks": resp.data,
            "meta": resp.meta,
        }),
    ))
}
