//! `taskfast artifact` — list / get / upload / delete artifacts on a task.
//!
//! Upload is also available via `taskfast task submit --artifact` (the
//! workflow verb), but a standalone verb is needed for:
//!   * Out-of-band uploads during dispute remedy.
//!   * Direct file-management workflows (curator attaching reference assets).
//!   * Deletion of a mistakenly uploaded artifact.
//!
//! The delete endpoint returns 204 (no body); we surface an empty success
//! envelope with the task/artifact IDs echoed for orchestrator ergonomics.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::map_api_error;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List artifacts attached to a task.
    List(ListArgs),
    /// Get a single artifact's metadata by ID.
    Get(GetArgs),
    /// Upload an artifact file. Content type is inferred from extension.
    Upload(UploadArgs),
    /// Delete an artifact by ID.
    Delete(GetArgs),
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
pub struct GetArgs {
    pub task_id: String,

    pub artifact_id: String,
}

#[derive(Debug, Parser)]
pub struct UploadArgs {
    pub task_id: String,

    /// Path to the file to upload.
    pub file: PathBuf,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::List(a) => list(ctx, a).await,
        Command::Get(a) => get(ctx, a).await,
        Command::Upload(a) => upload(ctx, a).await,
        Command::Delete(a) => delete(ctx, a).await,
    }
}

async fn list(ctx: &Ctx, args: ListArgs) -> CmdResult {
    let task_id = parse_uuid(&args.task_id, "task id")?;
    let client = ctx.client()?;
    let resp = match client
        .inner()
        .list_artifacts(&task_id, args.cursor.as_deref(), args.limit)
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
            "artifacts": resp.data,
            "meta": resp.meta,
        }),
    ))
}

async fn get(ctx: &Ctx, args: GetArgs) -> CmdResult {
    let task_id = parse_uuid(&args.task_id, "task id")?;
    let artifact_id = parse_uuid(&args.artifact_id, "artifact id")?;
    let client = ctx.client()?;
    let resp = match client.inner().get_artifact(&task_id, &artifact_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "artifact": resp }),
    ))
}

async fn upload(ctx: &Ctx, args: UploadArgs) -> CmdResult {
    let task_id = parse_uuid(&args.task_id, "task id")?;
    if !args.file.exists() {
        return Err(CmdError::Usage(format!(
            "artifact file not found: {}",
            args.file.display()
        )));
    }
    let filename = args
        .file
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| CmdError::Usage(format!("no filename in {}", args.file.display())))?
        .to_string();
    let ext = args
        .file
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let content_type = super::task::content_type_for_ext(&ext).to_string();

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_upload_artifact",
                "task_id": task_id.to_string(),
                "filename": filename,
                "content_type": content_type,
                "path": args.file.display().to_string(),
            }),
        ));
    }

    let client = ctx.client()?;
    let bytes = std::fs::read(&args.file)
        .map_err(|e| CmdError::Usage(format!("read {}: {e}", args.file.display())))?;
    let artifact = client
        .upload_artifact(&task_id, filename, content_type, bytes)
        .await
        .map_err(CmdError::from)?;
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": task_id.to_string(),
            "artifact": {
                "id": artifact.id,
                "filename": artifact.filename,
                "content_type": artifact.content_type,
                "size_bytes": artifact.size_bytes,
            },
        }),
    ))
}

async fn delete(ctx: &Ctx, args: GetArgs) -> CmdResult {
    let task_id = parse_uuid(&args.task_id, "task id")?;
    let artifact_id = parse_uuid(&args.artifact_id, "artifact id")?;
    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_delete_artifact",
                "task_id": task_id.to_string(),
                "artifact_id": artifact_id.to_string(),
            }),
        ));
    }
    let client = ctx.client()?;
    match client.inner().delete_artifact(&task_id, &artifact_id).await {
        Ok(_) => {}
        Err(e) => return Err(map_api_error(e).await.into()),
    }
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": task_id.to_string(),
            "artifact_id": artifact_id.to_string(),
            "deleted": true,
        }),
    ))
}

fn parse_uuid(raw: &str, label: &str) -> Result<Uuid, CmdError> {
    Uuid::parse_str(raw).map_err(|e| CmdError::Usage(format!("{label} must be a UUID: {e}")))
}
