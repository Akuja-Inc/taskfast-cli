// SPDX-License-Identifier: MIT
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::U256;
use clap::Parser;
use serde_json::{json, Value};
use uuid::Uuid;

use super::escrow::WalletOpts;
use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_chains::tempo::{sign_distribution, DistributionDomain};
use taskfast_client::api::types::RecordPayoutApprovalBody;
use taskfast_client::map_api_error;

const APPROVAL_LIFETIME_SECS: u64 = 36 * 3600;

#[derive(Debug, Parser)]
pub struct Args {
    /// Task UUID. Must be `complete` and posted by this agent.
    pub task_id: String,

    /// Keystore reference (same form as `taskfast settle`).
    #[arg(long, env = "TEMPO_KEY_SOURCE")]
    pub keystore: Option<String>,

    /// Path to keystore password file.
    #[arg(long, env = "TASKFAST_WALLET_PASSWORD_FILE")]
    pub wallet_password_file: Option<PathBuf>,

    /// Wallet address preflight (0x-prefixed): fail if the keystore differs.
    #[arg(long, env = "TEMPO_WALLET_ADDRESS")]
    pub wallet_address: Option<String>,

    /// Tempo RPC override. Defaults to the deployment's authenticated proxy.
    #[arg(long, env = "TEMPO_RPC_URL")]
    pub rpc_url: Option<String>,
}

pub async fn run(ctx: &Ctx, args: Args) -> CmdResult {
    let task_id = Uuid::parse_str(&args.task_id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    let opts = WalletOpts {
        keystore: args.keystore.as_deref(),
        password_file: args.wallet_password_file.as_deref(),
        wallet_address: args.wallet_address.as_deref(),
        rpc_url: args.rpc_url.as_deref(),
        receipt_timeout: None,
    };
    let data = record(ctx, &task_id, &opts, false).await?;
    Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
}

pub(super) async fn after_approve(
    ctx: &Ctx,
    task_id: &Uuid,
    opts: &WalletOpts<'_>,
) -> Result<Value, CmdError> {
    record(ctx, task_id, opts, true).await.map_err(|e| {
        CmdError::ApprovedPayoutNotSigned(format!(
            "task {task_id} IS approved (the server accepted it), but the payout approval was \
             NOT recorded: {e}. Do not approve again. Retry only this step: \
             `taskfast escrow payout-approval {task_id}` (safe to run more than once)"
        ))
    })
}

fn not_needed(note: &str) -> Value {
    json!({ "action": "not_needed", "note": note })
}

async fn record(
    ctx: &Ctx,
    task_id: &Uuid,
    opts: &WalletOpts<'_>,
    auto: bool,
) -> Result<Value, CmdError> {
    let client = ctx.client()?;
    let task = match client.inner().get_task(task_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    if auto && task.escrow_id.is_none() {
        return Ok(json!({ "action": "skipped", "reason": "task has no escrow" }));
    }
    match task.status.as_deref().unwrap_or("") {
        "complete" => {}
        "settled" => return Ok(not_needed("already paid")),
        status => {
            return Err(CmdError::Validation {
                code: "task_not_approved".into(),
                message: format!(
                    "task status is `{status}`; a payout approval is recorded only for an \
                     approved (`complete`) task. Nothing signed"
                ),
            })
        }
    }
    let target = super::escalate::chain_target(&task, "nothing to approve")?;
    let (rpc, _) = super::escalate::connect_rpc(ctx, &client, &target, opts.rpc_url).await?;
    let window = super::escrow::review_window(&rpc, target.contract, target.escrow_id).await?;
    if window.is_zero() {
        return Ok(json!({
            "action": "no_review_clock",
            "task_id": task_id.to_string(),
            "note": "this escrow has no review clock; no payout approval is needed",
        }));
    }

    let keystore_path =
        super::wallet_args::resolve_keystore_path(opts.keystore, ctx.keystore_path.as_deref());
    let signer = super::wallet_args::load_signer_and_validate(
        keystore_path.as_deref(),
        opts.password_file,
        opts.wallet_address.or(ctx.wallet_address.as_deref()),
        "payout approval",
    )?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| CmdError::Signing(format!("system clock is before the epoch: {e}")))?
        .as_secs();
    let deadline = now + APPROVAL_LIFETIME_SECS;
    let domain = DistributionDomain::new(target.chain_id_u64, target.contract);
    let signature = sign_distribution(&signer, &domain, target.escrow_id, U256::from(deadline))?;
    let signed = json!({
        "task_id": task_id.to_string(),
        "escrow_id": format!("{:#x}", target.escrow_id),
        "from": format!("{:#x}", signer.address()),
        "deadline": deadline,
    });

    if ctx.dry_run {
        let mut data = signed;
        data["action"] = json!("would_record_payout_approval");
        data["signature"] = json!(signature);
        return Ok(data);
    }

    let body = RecordPayoutApprovalBody {
        signature,
        deadline: i64::try_from(deadline)
            .map_err(|_| CmdError::Signing("approval deadline overflows i64".into()))?,
    };
    match client.inner().record_payout_approval(task_id, &body).await {
        Ok(_) => {
            let mut data = signed;
            data["action"] = json!("payout_approval_recorded");
            Ok(data)
        }
        Err(e) => match map_api_error(e).await {
            taskfast_client::Error::Validation { code, .. }
                if code == "payout_not_awaiting_approval" =>
            {
                let paid = client
                    .inner()
                    .get_task(task_id)
                    .await
                    .ok()
                    .map(|t| t.into_inner())
                    .and_then(|t| t.status)
                    .as_deref()
                    == Some("settled");
                Ok(not_needed(if paid {
                    "already paid"
                } else {
                    "the server needs no new approval now: one is stored or a payout is underway"
                }))
            }
            taskfast_client::Error::Validation { code, message }
                if code == "approval_lifetime_out_of_range" =>
            {
                Err(CmdError::Validation {
                    code,
                    message: format!(
                        "{message}; the server measures from its own clock, so check this \
                         machine's clock"
                    ),
                })
            }
            other => Err(other.into()),
        },
    }
}
