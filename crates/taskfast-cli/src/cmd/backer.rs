//! `taskfast backer` — manage an operator's external-backer allowlist
//! (gh#54 Stream B, server #483 `OperatorBackerController`).
//!
//! These are owning-user operations: they authenticate with a user PAT
//! (`tf_user_*`), NOT an agent key. Supply it via `--human-api-key` or
//! `TASKFAST_HUMAN_API_KEY`; it falls back to the standard `--api-key` only if
//! that already holds a user PAT (an agent key is rejected by the server).
//!
//! `operator_id` is the operator's PK UUID. The platform has no "get my
//! operator" endpoint yet, so it must be passed explicitly with `--operator` —
//! you receive it in the `POST /operators` activation response.

use clap::{Args, Subcommand};
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::api::types::CreateOperatorBackerBody;
use taskfast_client::{map_api_error, TaskFastClient};

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List the operator's approved backers.
    List(ListArgs),
    /// Approve a backer (account + wallet) for the operator.
    Add(AddArgs),
    /// Revoke an approved backer (blocks new stakes; posted bonds stand).
    Revoke(RevokeArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Operator PK UUID (from the `POST /operators` activation response).
    #[arg(long)]
    pub operator: String,
    /// User PAT (`tf_user_*`). Falls back to the standard API key.
    #[arg(long, env = "TASKFAST_HUMAN_API_KEY")]
    pub human_api_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Operator PK UUID.
    #[arg(long)]
    pub operator: String,
    /// Backer account UUID (must be a human account).
    #[arg(long)]
    pub account: String,
    /// Wallet the backer posts stakes from (`0x`-prefixed).
    #[arg(long)]
    pub wallet: String,
    /// User PAT (`tf_user_*`). Falls back to the standard API key.
    #[arg(long, env = "TASKFAST_HUMAN_API_KEY")]
    pub human_api_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct RevokeArgs {
    /// Operator PK UUID.
    #[arg(long)]
    pub operator: String,
    /// Backer allowlist entry UUID (the `id` from `backer list`).
    #[arg(long)]
    pub id: String,
    /// User PAT (`tf_user_*`). Falls back to the standard API key.
    #[arg(long, env = "TASKFAST_HUMAN_API_KEY")]
    pub human_api_key: Option<String>,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::List(a) => list(ctx, a).await,
        Command::Add(a) => add(ctx, a).await,
        Command::Revoke(a) => revoke(ctx, a).await,
    }
}

/// Build a client from the user PAT. Prefers `--human-api-key`
/// (`TASKFAST_HUMAN_API_KEY`), then the standard API key, so a caller whose
/// `TASKFAST_API_KEY` is already a user PAT needn't pass the flag twice.
fn pat_client(ctx: &Ctx, human_api_key: Option<&str>) -> Result<TaskFastClient, CmdError> {
    let key = human_api_key
        .filter(|k| !k.trim().is_empty())
        .or(ctx.api_key.as_deref())
        .ok_or(CmdError::MissingApiKey)?;
    TaskFastClient::from_api_key(ctx.base_url(), key).map_err(CmdError::from)
}

fn parse_operator(s: &str) -> Result<Uuid, CmdError> {
    Uuid::parse_str(s).map_err(|e| CmdError::Usage(format!("--operator must be a UUID: {e}")))
}

async fn list(ctx: &Ctx, args: ListArgs) -> CmdResult {
    // A read — `--dry-run` only short-circuits mutations, so this always runs.
    let operator_id = parse_operator(&args.operator)?;
    let client = pat_client(ctx, args.human_api_key.as_deref())?;
    let resp = match client.inner().list_operator_backers(&operator_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "backers": resp.backers }),
    ))
}

async fn add(ctx: &Ctx, args: AddArgs) -> CmdResult {
    let operator_id = parse_operator(&args.operator)?;
    let backer_account_id = Uuid::parse_str(&args.account)
        .map_err(|e| CmdError::Usage(format!("--account must be a UUID: {e}")))?;
    if args.wallet.trim().is_empty() {
        return Err(CmdError::Usage("--wallet must not be empty".into()));
    }

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_add_backer",
                "operator_id": operator_id.to_string(),
                "backer_account_id": backer_account_id.to_string(),
                "wallet_address": args.wallet,
            }),
        ));
    }

    let client = pat_client(ctx, args.human_api_key.as_deref())?;
    let body = CreateOperatorBackerBody {
        backer_account_id,
        wallet_address: args.wallet.clone(),
    };
    let resp = match client
        .inner()
        .create_operator_backer(&operator_id, &body)
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "backer": resp }),
    ))
}

async fn revoke(ctx: &Ctx, args: RevokeArgs) -> CmdResult {
    let operator_id = parse_operator(&args.operator)?;
    let backer_id = Uuid::parse_str(&args.id)
        .map_err(|e| CmdError::Usage(format!("--id must be a UUID: {e}")))?;

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_revoke_backer",
                "operator_id": operator_id.to_string(),
                "id": backer_id.to_string(),
            }),
        ));
    }

    let client = pat_client(ctx, args.human_api_key.as_deref())?;
    let resp = match client
        .inner()
        .revoke_operator_backer(&operator_id, &backer_id)
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "backer": resp }),
    ))
}
