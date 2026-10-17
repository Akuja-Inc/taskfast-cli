// SPDX-License-Identifier: MIT
//! `taskfast artifact` — list / get / upload / delete artifacts on a task,
//! plus non-custodial CID submission (`cid` / `cid-status`).
//!
//! Upload is also available via `taskfast task submit --artifact` (the
//! workflow verb), but a standalone verb is needed for:
//!   * Out-of-band uploads during dispute remedy.
//!   * Direct file-management workflows (curator attaching reference assets).
//!   * Deletion of a mistakenly uploaded artifact.
//!
//! The delete endpoint returns 204 (no body); we surface an empty success
//! envelope with the task/artifact IDs echoed for orchestrator ergonomics.
//!
//! `cid` records an externally-pinned CIDv1 as the delivery artifact —
//! TaskFast never transits the bytes. `cid-status` is the buyer-side verdict
//! (`witnessed` / `unverifiable`); only the task's poster may write it.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;
use crate::Environment;

use taskfast_client::api::types::{
    Artifact, SubmitArtifactCidBody, UpdateArtifactCidStatusBody,
    UpdateArtifactCidStatusBodyCidStatus,
};
use taskfast_client::{map_api_error, put_presigned_upload, ArtifactUpload, TaskFastClient};

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
    /// Register an externally-pinned CIDv1 as the delivery artifact.
    Cid(CidArgs),
    /// Set a CID artifact's buyer-verifier verdict (poster only).
    CidStatus(CidStatusArgs),
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

#[derive(Debug, Parser)]
pub struct CidArgs {
    pub task_id: String,

    /// CIDv1 base32 lowercase using sha2-256 (`bafy…` / `bafk…`). The server
    /// rejects CIDv0 (`Qm…`), non-sha256 multihashes, and non-canonical forms.
    pub output_cid: String,
}

/// CLI mirror of the generated [`UpdateArtifactCidStatusBodyCidStatus`] enum.
/// Defined locally so clap's `ValueEnum` validates the verdict at parse time
/// (with the allowed values in `--help`) before any network call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CidStatusArg {
    /// Default verdict written by the operator's submission.
    Witnessed,
    /// Buyer-verifier verdict after a failed off-system fetch.
    Unverifiable,
}

impl From<CidStatusArg> for UpdateArtifactCidStatusBodyCidStatus {
    fn from(v: CidStatusArg) -> Self {
        match v {
            CidStatusArg::Witnessed => Self::Witnessed,
            CidStatusArg::Unverifiable => Self::Unverifiable,
        }
    }
}

#[derive(Debug, Parser)]
pub struct CidStatusArgs {
    pub task_id: String,

    pub artifact_id: String,

    /// Verdict to persist.
    pub status: CidStatusArg,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::List(a) => list(ctx, a).await,
        Command::Get(a) => get(ctx, a).await,
        Command::Upload(a) => upload(ctx, a).await,
        Command::Delete(a) => delete(ctx, a).await,
        Command::Cid(a) => submit_cid(ctx, a).await,
        Command::CidStatus(a) => update_cid_status(ctx, a).await,
    }
}

async fn list(ctx: &Ctx, args: ListArgs) -> CmdResult {
    let task_id = parse_uuid(&args.task_id, "task id")?;
    let client = ctx.client()?;
    let resp = match client
        .inner()
        .list_artifacts(
            &task_id,
            args.cursor.as_deref(),
            args.limit.and_then(taskfast_client::page_limit),
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
    let artifact =
        upload_file(ctx, &client, &task_id, &args.file, &filename, &content_type).await?;
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

/// Upload one file as a task artifact (gh#193): ask the server for a
/// presigned PUT, send the bytes straight to storage, then complete.
///
/// Multipart through the app is a development-only fallback (`--env local`,
/// whose Local storage adapter answers `multipart`, or an older dev server
/// that 404s). On staging and production that path would take the service
/// down, so it is refused there rather than retried.
pub(super) async fn upload_file(
    ctx: &Ctx,
    client: &TaskFastClient,
    task_id: &Uuid,
    path: &Path,
    filename: &str,
    content_type: &str,
) -> Result<Artifact, CmdError> {
    let read_err = |e: std::io::Error| CmdError::Usage(format!("read {}: {e}", path.display()));
    let file = std::fs::File::open(path).map_err(read_err)?;
    let size_bytes = file.metadata().map_err(read_err)?.len();
    match client
        .initiate_artifact_upload(task_id, filename, content_type, size_bytes)
        .await?
    {
        Some(ArtifactUpload::Put {
            upload_id,
            url,
            headers,
            expires_at,
        }) => {
            check_upload_url(&url, ctx.allow_custom_endpoints)?;
            put_presigned_upload(&url, &headers, file, size_bytes).await?;
            Ok(client
                .complete_artifact_upload(task_id, &upload_id, expires_at)
                .await?)
        }
        _ if ctx.environment == Environment::Local => {
            let bytes = std::fs::read(path).map_err(read_err)?;
            Ok(client
                .upload_artifact(task_id, filename.into(), content_type.into(), bytes)
                .await?)
        }
        _ => Err(CmdError::Server(
            "this server does not offer direct uploads".into(),
        )),
    }
}

/// Pin presigned upload URLs to R2, so a server bug or tampered response
/// cannot aim artifact bytes at another host. Bypassed only by
/// `--allow-custom-endpoints`, like the API endpoint guard.
fn check_upload_url(url: &str, allow_custom: bool) -> Result<(), CmdError> {
    let is_r2 = reqwest::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str()
                .is_some_and(|h| h.ends_with(".r2.cloudflarestorage.com"))
    });
    if is_r2 || allow_custom {
        return Ok(());
    }
    Err(CmdError::Usage(
        "refusing presigned upload URL: must be https on *.r2.cloudflarestorage.com".into(),
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

async fn submit_cid(ctx: &Ctx, args: CidArgs) -> CmdResult {
    let task_id = parse_uuid(&args.task_id, "task id")?;
    // Reject an empty CID pre-HTTP so orchestrators see a never-retry Usage
    // error rather than a server-side 422. The server enforces the full
    // CIDv1/sha2-256 subset; we only guard the trivial empty case here.
    let output_cid = args.output_cid.trim();
    if output_cid.is_empty() {
        return Err(CmdError::Usage("output_cid must not be empty".into()));
    }
    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_submit_cid",
                "task_id": task_id.to_string(),
                "output_cid": output_cid,
            }),
        ));
    }
    let client = ctx.client()?;
    let body = SubmitArtifactCidBody {
        output_cid: output_cid.to_string(),
    };
    let resp = match client.inner().submit_artifact_cid(&task_id, &body).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": task_id.to_string(),
            "artifact": resp,
        }),
    ))
}

async fn update_cid_status(ctx: &Ctx, args: CidStatusArgs) -> CmdResult {
    let task_id = parse_uuid(&args.task_id, "task id")?;
    let artifact_id = parse_uuid(&args.artifact_id, "artifact id")?;
    let cid_status: UpdateArtifactCidStatusBodyCidStatus = args.status.into();
    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_update_cid_status",
                "task_id": task_id.to_string(),
                "artifact_id": artifact_id.to_string(),
                "cid_status": cid_status.to_string(),
            }),
        ));
    }
    let client = ctx.client()?;
    let body = UpdateArtifactCidStatusBody { cid_status };
    let resp = match client
        .inner()
        .update_artifact_cid_status(&task_id, &artifact_id, &body)
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
            "artifact": resp,
        }),
    ))
}

fn parse_uuid(raw: &str, label: &str) -> Result<Uuid, CmdError> {
    Uuid::parse_str(raw).map_err(|e| CmdError::Usage(format!("{label} must be a UUID: {e}")))
}

#[cfg(test)]
mod tests {
    use super::check_upload_url;

    #[test]
    fn upload_url_must_be_https_r2() {
        assert!(
            check_upload_url("https://acct.r2.cloudflarestorage.com/pending/x?s=1", false).is_ok()
        );
        for bad in [
            "http://acct.r2.cloudflarestorage.com/pending/x",
            "https://acct.r2.cloudflarestorage.com.evil.com/x",
            "https://acct.r2.cloudflarestorage.com@evil.com/x",
            "https://r2.cloudflarestorage.com/x",
            "https://evil.com/acct.r2.cloudflarestorage.com",
            "not a url",
        ] {
            assert!(check_upload_url(bad, false).is_err(), "{bad}");
        }
        assert!(check_upload_url("http://127.0.0.1:9/x", true).is_ok());
    }
}
