//! `taskfast review` — post-task reputation messages.
//!
//! `create` writes a 1–5 star review (with comment) for the opposite party
//! on a completed task. `list --task` fetches all reviews on a task (both
//! directions); `list --agent` fetches every review authored for an agent.

use clap::{Parser, Subcommand};
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::api::types::{ReviewCreateRequest, ReviewCreateRequestComment};
use taskfast_client::map_api_error;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Write a review on a completed task.
    Create(CreateArgs),
    /// List reviews either for a task or for an agent.
    List(ListArgs),
}

#[derive(Debug, Parser)]
pub struct CreateArgs {
    pub task_id: String,

    /// UUID of the account being reviewed.
    #[arg(long)]
    pub reviewee_id: String,

    /// Star rating 1..=5.
    #[arg(long)]
    pub rating: i64,

    /// Free-form written review.
    #[arg(long)]
    pub comment: String,
}

#[derive(Debug, Parser)]
pub struct ListArgs {
    /// List reviews on a task (both directions). Mutually exclusive with `--agent`.
    #[arg(long, conflicts_with = "agent")]
    pub task: Option<String>,

    /// List reviews authored for an agent. Mutually exclusive with `--task`.
    #[arg(long)]
    pub agent: Option<String>,

    #[arg(long)]
    pub cursor: Option<String>,

    #[arg(long)]
    pub limit: Option<i64>,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Create(a) => create(ctx, a).await,
        Command::List(a) => list(ctx, a).await,
    }
}

async fn create(ctx: &Ctx, args: CreateArgs) -> CmdResult {
    let task_id = Uuid::parse_str(&args.task_id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    let reviewee_id = Uuid::parse_str(&args.reviewee_id)
        .map_err(|e| CmdError::Usage(format!("--reviewee-id must be a UUID: {e}")))?;
    if !(1..=5).contains(&args.rating) {
        return Err(CmdError::Usage("--rating must be an integer 1..=5".into()));
    }
    if args.comment.trim().is_empty() {
        return Err(CmdError::Usage("--comment must not be empty".into()));
    }
    let comment = ReviewCreateRequestComment::try_from(args.comment.as_str())
        .map_err(|e| CmdError::Usage(format!("--comment rejected by schema: {e}")))?;

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_create_review",
                "task_id": task_id.to_string(),
                "reviewee_id": reviewee_id.to_string(),
                "rating": args.rating,
                "comment": args.comment,
            }),
        ));
    }

    let body = ReviewCreateRequest {
        comment,
        rating: args.rating,
        reviewee_id,
    };
    let client = ctx.client()?;
    let resp = match client.inner().create_review(&task_id, &body).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "review": resp }),
    ))
}

async fn list(ctx: &Ctx, args: ListArgs) -> CmdResult {
    // Exactly one axis must be supplied — server endpoints diverge.
    let client = ctx.client()?;
    match (args.task.as_deref(), args.agent.as_deref()) {
        (Some(t), None) => {
            let task_id = Uuid::parse_str(t)
                .map_err(|e| CmdError::Usage(format!("--task must be a UUID: {e}")))?;
            let resp = match client
                .inner()
                .list_task_reviews(&task_id, args.cursor.as_deref(), args.limit)
                .await
            {
                Ok(v) => v.into_inner(),
                Err(e) => return Err(map_api_error(e).await.into()),
            };
            Ok(Envelope::success(
                ctx.environment,
                ctx.dry_run,
                json!({
                    "task_id": task_id.to_string(),
                    "reviews": resp.data,
                    "meta": resp.meta,
                }),
            ))
        }
        (None, Some(a)) => {
            let agent_id = Uuid::parse_str(a)
                .map_err(|e| CmdError::Usage(format!("--agent must be a UUID: {e}")))?;
            let resp = match client
                .inner()
                .list_agent_reviews(&agent_id, args.cursor.as_deref(), args.limit)
                .await
            {
                Ok(v) => v.into_inner(),
                Err(e) => return Err(map_api_error(e).await.into()),
            };
            Ok(Envelope::success(
                ctx.environment,
                ctx.dry_run,
                json!({
                    "agent_id": agent_id.to_string(),
                    "reviews": resp.data,
                    "meta": resp.meta,
                }),
            ))
        }
        (None, None) => Err(CmdError::Usage(
            "review list requires --task <id> or --agent <id>".into(),
        )),
        (Some(_), Some(_)) => unreachable!("clap conflicts_with enforces this"),
    }
}
