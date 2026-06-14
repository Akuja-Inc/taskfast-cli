//! `taskfast stake <task_id>` — operator posts a performance stake on a
//! direct high-assurance task (gh#54, server #482 `POST /tasks/{id}/stake`).
//!
//! Thin agent-key POST: with on-chain bond posting disabled (the current
//! deployment default) the server records the bond and finalizes the
//! assignment in-band, so there is no EIP-712 signing here — unlike
//! `escrow sign`. When posting is enabled the bond is parked
//! (`status: awaiting_verification`) and the operator submits the on-chain
//! `TaskBond.post` tx out-of-band; that reporting path is tracked separately
//! and is out of scope for this command.
//!
//! `--source external-backer` authenticates as an approved backer and requires
//! `--wallet`; the server enforces the operator's backer allowlist.

use clap::{Parser, ValueEnum};
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::api::types::{PostStakeBody, PostStakeBodyStakeSource};
use taskfast_client::map_api_error;

/// clap-friendly mirror of the generated `PostStakeBodyStakeSource` enum.
/// Kept local (like `bid::BidStatusFilter`) so the codegen needn't grow clap
/// derives; kebab-case renders `operator-self` / `external-backer`.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum StakeSource {
    OperatorSelf,
    ExternalBacker,
}

impl StakeSource {
    fn to_api(self) -> PostStakeBodyStakeSource {
        match self {
            Self::OperatorSelf => PostStakeBodyStakeSource::OperatorSelf,
            Self::ExternalBacker => PostStakeBodyStakeSource::ExternalBacker,
        }
    }

    /// Wire value, for the `--dry-run` echo (matches the JSON the server sees).
    fn as_str(self) -> &'static str {
        match self {
            Self::OperatorSelf => "operator_self",
            Self::ExternalBacker => "external_backer",
        }
    }
}

#[derive(Debug, Parser)]
pub struct Args {
    /// Target task UUID. Must be a direct high-assurance task parked at
    /// `awaiting_stake` whose Operator of Record you belong to.
    pub task_id: String,

    /// Stake amount in bond **base units** (integer, not a decimal). Must meet
    /// the deployment's high-assurance minimum — the server owns that floor.
    #[arg(long)]
    pub amount: i64,

    /// Who is posting. `operator-self` (default) authenticates with your agent
    /// key; `external-backer` posts on the operator's behalf and needs `--wallet`.
    #[arg(long, value_enum, default_value_t = StakeSource::OperatorSelf)]
    pub source: StakeSource,

    /// Approved backer wallet (`0x`-prefixed). Required with
    /// `--source external-backer`; ignored otherwise.
    #[arg(long)]
    pub wallet: Option<String>,
}

pub async fn run(ctx: &Ctx, args: Args) -> CmdResult {
    // Fail on a bad UUID before any HTTP so typos never cost a round-trip.
    let task_id = Uuid::parse_str(&args.task_id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;

    // Reject a non-positive amount upfront. The server owns the high-assurance
    // floor, but `minimum: 1` is structural — no point spending a 422 on it.
    if args.amount < 1 {
        return Err(CmdError::Usage(
            "--amount must be a positive integer (bond base units)".into(),
        ));
    }

    // External-backer staking requires a posting wallet; fail fast so an
    // orchestrator sees a never-retry Usage error rather than a server 422.
    let wallet_address = match args.source {
        StakeSource::ExternalBacker => match args.wallet.as_deref() {
            Some(w) if !w.trim().is_empty() => Some(w.to_string()),
            _ => {
                return Err(CmdError::Usage(
                    "--wallet is required with --source external-backer".into(),
                ));
            }
        },
        // The server ignores `wallet_address` for operator-self; don't send it.
        StakeSource::OperatorSelf => None,
    };

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_stake",
                "task_id": task_id.to_string(),
                "amount": args.amount,
                "stake_source": args.source.as_str(),
                "wallet_address": wallet_address,
            }),
        ));
    }

    let client = ctx.client()?;
    let body = PostStakeBody {
        amount: args.amount,
        stake_source: args.source.to_api(),
        wallet_address,
    };
    let resp = match client.inner().post_stake(&task_id, &body).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "stake": resp }),
    ))
}
