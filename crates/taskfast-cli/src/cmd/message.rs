//! `taskfast message` — send + read task-scoped messages.
//!
//! Conversations are keyed by task; `list-conversations` returns the task's
//! distinct participant threads, `list` returns messages in a thread, and
//! `send` appends a new message. The API is symmetric between poster and
//! worker — server infers role from the API key.

use clap::{Parser, Subcommand};
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::api::types::{MessageSendRequest, MessageSendRequestContent};
use taskfast_client::map_api_error;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Send a message on a task.
    Send(SendArgs),
    /// List messages in a task's thread.
    List(ListArgs),
    /// List the task's distinct conversations.
    Conversations(ConversationsArgs),
}

#[derive(Debug, Parser)]
pub struct SendArgs {
    pub task_id: String,

    /// Message body (plain text). Accepts whatever the server allows — no
    /// local length cap because server-side policy has historically changed.
    #[arg(long)]
    pub content: String,
}

#[derive(Debug, Parser)]
pub struct ListArgs {
    pub task_id: String,

    #[arg(long)]
    pub cursor: Option<String>,

    #[arg(long)]
    pub limit: Option<i64>,
}

#[derive(Debug, Parser)]
pub struct ConversationsArgs {
    pub task_id: String,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Send(a) => send(ctx, a).await,
        Command::List(a) => list(ctx, a).await,
        Command::Conversations(a) => conversations(ctx, a).await,
    }
}

async fn send(ctx: &Ctx, args: SendArgs) -> CmdResult {
    let task_id = Uuid::parse_str(&args.task_id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    if args.content.trim().is_empty() {
        return Err(CmdError::Usage("--content must not be empty".into()));
    }
    let content = MessageSendRequestContent::try_from(args.content.as_str())
        .map_err(|e| CmdError::Usage(format!("--content rejected by schema: {e}")))?;

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_send_message",
                "task_id": task_id.to_string(),
                "content": args.content,
            }),
        ));
    }

    let body = MessageSendRequest { content };
    let client = ctx.client()?;
    let resp = match client.inner().send_message(&task_id, &body).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "message": resp }),
    ))
}

async fn list(ctx: &Ctx, args: ListArgs) -> CmdResult {
    let task_id = Uuid::parse_str(&args.task_id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    let client = ctx.client()?;
    let resp = match client
        .inner()
        .list_messages(&task_id, args.cursor.as_deref(), args.limit)
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
            "messages": resp.data,
            "meta": resp.meta,
        }),
    ))
}

async fn conversations(ctx: &Ctx, args: ConversationsArgs) -> CmdResult {
    let task_id = Uuid::parse_str(&args.task_id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    let client = ctx.client()?;
    let resp = match client.inner().list_conversations(&task_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": task_id.to_string(),
            "conversations": resp,
        }),
    ))
}
