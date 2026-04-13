//! `taskfast task` — read + mutate operations on tasks.
//!
//! This slice (am-e3u.4) implements the **read path** only: `list` + `get`.
//! Mutations (`submit`, `approve`, `dispute`, `cancel`) stay as
//! `Unimplemented` stubs so `main.rs` dispatch keeps compiling; each one
//! lands in its own bead with signing/escrow concerns handled in isolation.
//!
//! # List semantics
//!
//! Three server endpoints hide behind `--kind`:
//!
//! | `--kind`  | Endpoint                       | Response         |
//! |-----------|--------------------------------|------------------|
//! | `mine`    | `GET /agents/me/tasks`         | worker's active workload (default; supports `--status`) |
//! | `queue`   | `GET /agents/me/queue`         | assigned-but-unclaimed work |
//! | `posted`  | `GET /agents/me/posted_tasks`  | tasks this agent posted |
//!
//! `--status` is only meaningful with `--kind=mine`; supplying it with any
//! other kind is a [`CmdError::Usage`] rather than a silent no-op (ambiguous
//! flag combinations should fail loud).

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::TaskFastClient;
use taskfast_client::api::types::ListMyTasksStatus;
use taskfast_client::map_api_error;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List tasks (see `--kind` for which collection).
    List(ListArgs),
    /// GET /tasks/{id} — full task detail.
    Get(GetArgs),
    /// Worker: submit a completion. (Deferred — see am-e3u.5.)
    Submit {
        id: String,
        #[arg(long)]
        artifact: Vec<String>,
        #[arg(long)]
        summary: String,
    },
    /// Poster: approve a submission. (Deferred — needs settle flow; see am-e3u.8.)
    Approve { id: String },
    /// Either side: open a dispute. (Deferred — see am-e3u.6.)
    Dispute {
        id: String,
        #[arg(long)]
        reason: String,
    },
    /// Poster: cancel before assignment. (Deferred — see am-e3u.7.)
    Cancel { id: String },
}

#[derive(Debug, Parser)]
pub struct ListArgs {
    /// Which collection to list. See module docs for the endpoint mapping.
    #[arg(long, default_value = "mine")]
    pub kind: ListKind,

    /// Filter by task status. Only valid with `--kind=mine`; supplying it
    /// with another kind is a usage error.
    #[arg(long)]
    pub status: Option<TaskStatus>,

    /// Opaque pagination cursor from a previous response's `next_cursor`.
    #[arg(long)]
    pub cursor: Option<String>,

    /// Max items per page. Server enforces its own ceiling; we pass through.
    #[arg(long)]
    pub limit: Option<i64>,
}

#[derive(Debug, Parser)]
pub struct GetArgs {
    /// Task ID (UUID).
    pub id: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ListKind {
    /// Tasks where the agent is the assigned worker (default).
    Mine,
    /// Assigned-but-unclaimed queue (subset of `mine` with server-specific shape).
    Queue,
    /// Tasks this agent has posted.
    Posted,
}

/// Mirror of `ListMyTasksStatus` carved as a clap-friendly `ValueEnum`.
///
/// The generated enum already derives `ValueEnum`-compatible serde, but
/// clap's `ValueEnum` needs kebab-case variants and the `Display` impl
/// already lives on the generated type — cheaper to keep a thin mirror here
/// than to teach clap about foreign traits.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum TaskStatus {
    InProgress,
    UnderReview,
    Disputed,
    Remedied,
    Assigned,
    All,
}

impl From<TaskStatus> for ListMyTasksStatus {
    fn from(s: TaskStatus) -> Self {
        match s {
            TaskStatus::InProgress => Self::InProgress,
            TaskStatus::UnderReview => Self::UnderReview,
            TaskStatus::Disputed => Self::Disputed,
            TaskStatus::Remedied => Self::Remedied,
            TaskStatus::Assigned => Self::Assigned,
            TaskStatus::All => Self::All,
        }
    }
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::List(args) => list(ctx, args).await,
        Command::Get(args) => get(ctx, args).await,
        Command::Submit { .. } => Err(CmdError::Unimplemented("taskfast task submit")),
        Command::Approve { .. } => Err(CmdError::Unimplemented("taskfast task approve")),
        Command::Dispute { .. } => Err(CmdError::Unimplemented("taskfast task dispute")),
        Command::Cancel { .. } => Err(CmdError::Unimplemented("taskfast task cancel")),
    }
}

async fn list(ctx: &Ctx, args: ListArgs) -> CmdResult {
    if args.status.is_some() && !matches!(args.kind, ListKind::Mine) {
        return Err(CmdError::Usage(
            "--status is only valid with --kind=mine".into(),
        ));
    }
    let client = ctx.client()?;
    let data = match args.kind {
        ListKind::Mine => list_mine(&client, &args).await?,
        ListKind::Queue => list_queue(&client, &args).await?,
        ListKind::Posted => list_posted(&client, &args).await?,
    };
    Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
}

async fn list_mine(client: &TaskFastClient, args: &ListArgs) -> Result<serde_json::Value, CmdError> {
    let status = args.status.map(ListMyTasksStatus::from);
    let resp = match client
        .inner()
        .list_my_tasks(args.cursor.as_deref(), args.limit, status)
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(json!({
        "kind": "mine",
        "tasks": resp.data,
        "meta": resp.meta,
    }))
}

async fn list_queue(
    client: &TaskFastClient,
    args: &ListArgs,
) -> Result<serde_json::Value, CmdError> {
    let resp = match client
        .inner()
        .get_agent_queue(args.cursor.as_deref(), args.limit)
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(json!({
        "kind": "queue",
        "tasks": resp.data,
        "meta": resp.meta,
    }))
}

async fn list_posted(
    client: &TaskFastClient,
    args: &ListArgs,
) -> Result<serde_json::Value, CmdError> {
    let resp = match client
        .inner()
        .get_agent_posted_tasks(args.cursor.as_deref(), args.limit)
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(json!({
        "kind": "posted",
        "tasks": resp.data,
        "meta": resp.meta,
    }))
}

async fn get(ctx: &Ctx, args: GetArgs) -> CmdResult {
    // Validate UUID locally — bad IDs shouldn't cost a round-trip.
    let id = Uuid::parse_str(&args.id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    let client = ctx.client()?;
    let task = match client.inner().get_task(&id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "task": task }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_maps_to_generated_enum() {
        // Pin the mapping — changing it would be a silent wire-shape change.
        for (ours, theirs) in [
            (TaskStatus::InProgress, ListMyTasksStatus::InProgress),
            (TaskStatus::UnderReview, ListMyTasksStatus::UnderReview),
            (TaskStatus::Disputed, ListMyTasksStatus::Disputed),
            (TaskStatus::Remedied, ListMyTasksStatus::Remedied),
            (TaskStatus::Assigned, ListMyTasksStatus::Assigned),
            (TaskStatus::All, ListMyTasksStatus::All),
        ] {
            // `ListMyTasksStatus: Display` — compare as strings to avoid
            // needing PartialEq on the foreign type.
            assert_eq!(
                ListMyTasksStatus::from(ours).to_string(),
                theirs.to_string()
            );
        }
    }
}
