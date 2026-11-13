// SPDX-License-Identifier: MIT
//! `taskfast escalate` — the poster's on-chain dispute path (taskfast#2044).
//!
//! After a Tier 1 or Tier 2 remedy fails, the dispute moves to the poster's own
//! wallet: TaskFast holds no key that can sign `dispute(bytes32)`.
//!
//!  - `escalate reject-remedy <task_id>` → `POST /tasks/{id}/dispute/reject-remedy`
//!    moves a dispute with a failed remedy to "awaiting the poster's signature".
//!  - `escalate sign <task_id>` → `GET /tasks/{id}/dispute/signing-params`, then
//!    signs and broadcasts `ArbitratedEscrow.dispute(escrowId)` from the poster's
//!    keystore. Every value comes from the server; the CLI computes none.
//!
//! The dispute counts as signed only when the server's reconciler sees the
//! finalized `EscrowDisputed` log, so output says "submitted", never "disputed".
//! The CLI never compares its own clock with `escalation_deadline`: with
//! `lapse_pending` true it warns and still sends, because the server accepts a
//! signature until it records the lapse.
//!
//! Refuse-to-sign guards (all before any transaction; signing safety is the
//! CLI's one unique job, so these deliberately do not trust the server blindly):
//!  1. keystore address ≠ a non-nil `poster_wallet_address`;
//!  2. `chain_id` ≠ the configured network's chain id, or ≠ the RPC node's;
//!  3. `task_escrow_contract` / `escrow_id` malformed, or the contract is zero;
//!  4. keystore address ≠ `--wallet-address` / configured wallet address.
//!
//! `--dry-run` runs guards 1–4 except the node check (it opens no RPC).

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, B256};
use clap::Parser;
use serde_json::json;
use uuid::Uuid;

use super::{
    is_proxy_rpc_url, network_policy_for_chain_id, validate_override_rpc_url, CmdError, CmdResult,
    Ctx,
};
use crate::envelope::{Envelope, SecurityWarning};

use taskfast_agent::chain::dispute_calldata;
use taskfast_agent::tempo_rpc::{sign_and_broadcast_tx, TempoRpcClient};
use taskfast_chains::tempo::TEMPO_MAINNET_CHAIN_ID;
use taskfast_client::map_api_error;

const RECEIPT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Same client-operational tuning as `escrow sign`: 3min mainnet, 1min otherwise.
fn default_receipt_timeout(chain_id: u64) -> Duration {
    if chain_id == TEMPO_MAINNET_CHAIN_ID {
        Duration::from_mins(3)
    } else {
        Duration::from_mins(1)
    }
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Poster: reject a failed remedy; the dispute then awaits your on-chain signature.
    RejectRemedy(RejectRemedyArgs),
    /// Poster: sign + broadcast `dispute(escrowId)` from your own wallet.
    Sign(SignArgs),
}

#[derive(Debug, Parser)]
pub struct RejectRemedyArgs {
    /// Task UUID. Caller must be the task's poster.
    pub task_id: String,
}

#[derive(Debug, Parser)]
pub struct SignArgs {
    /// Task UUID. The dispute must be awaiting the poster's signature.
    pub task_id: String,

    /// Keystore reference (same form as `taskfast escrow sign`).
    #[arg(long, env = "TEMPO_KEY_SOURCE")]
    pub keystore: Option<String>,

    /// Path to keystore password file.
    #[arg(long, env = "TASKFAST_WALLET_PASSWORD_FILE")]
    pub wallet_password_file: Option<PathBuf>,

    /// Poster wallet address preflight (0x-prefixed): fail if the keystore differs.
    #[arg(long, env = "TEMPO_WALLET_ADDRESS")]
    pub wallet_address: Option<String>,

    /// Tempo RPC override. Defaults to the deployment's authenticated proxy
    /// for the configured network (`GET /config/network`).
    #[arg(long, env = "TEMPO_RPC_URL")]
    pub rpc_url: Option<String>,

    /// Receipt-polling ceiling — human duration (`3min`, `90s`).
    #[arg(
        long,
        env = "TASKFAST_RECEIPT_TIMEOUT",
        value_parser = humantime::parse_duration,
    )]
    pub receipt_timeout: Option<Duration>,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::RejectRemedy(a) => reject_remedy(ctx, a).await,
        Command::Sign(a) => sign(ctx, a).await,
    }
}

fn parse_task_id(raw: &str) -> Result<Uuid, CmdError> {
    Uuid::parse_str(raw).map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))
}

/// One-line next step per server refusal. Hints only; the server's code and
/// text stay in the error so orchestrators can still branch on the code.
fn hint(code: &str) -> Option<&'static str> {
    Some(match code {
        "onchain_dispute_disabled" => "on-chain dispute signing is not enabled on this deployment",
        "remedy_not_rejectable" => "no failed remedy can be rejected on this dispute right now",
        "dispute_not_awaiting_signature" => {
            "the dispute is absent, already resolved, or not awaiting your signature; run `taskfast dispute <task_id>`"
        }
        "escrow_already_disputed" => "the escrow is already disputed on chain; nothing to sign",
        "escrow_not_held" => "the escrow is not held, so it cannot be disputed",
        _ => return None,
    })
}

/// Map a server error, appending the action hint for the known refusal codes.
fn refusal(err: taskfast_client::Error) -> CmdError {
    match err {
        taskfast_client::Error::Validation { code, message } => match hint(&code) {
            Some(h) => CmdError::Validation {
                message: format!("{message} → {h}"),
                code,
            },
            None => CmdError::Validation { code, message },
        },
        // The shared 5xx mapper keeps only the message, not the server's error
        // code, so this is a hint (our inference), never a `code` value.
        taskfast_client::Error::Server(m) if m.contains("503") => CmdError::Server(format!(
            "{m} → hint: on this endpoint a 503 normally means the escrow target is \
             unavailable (escrow_target_unavailable); retry later"
        )),
        other => other.into(),
    }
}

async fn reject_remedy(ctx: &Ctx, args: RejectRemedyArgs) -> CmdResult {
    let task_id = parse_task_id(&args.task_id)?;
    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({ "action": "would_reject_remedy", "task_id": task_id.to_string() }),
        ));
    }
    let client = ctx.client()?;
    let resp = match client.inner().reject_remedy(&task_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(refusal(map_api_error(e).await)),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": resp.task_id,
            "phase": resp.phase,
            "escalation_deadline": resp.escalation_deadline,
        }),
    ))
}

fn missing(field: &str) -> CmdError {
    CmdError::Decode(format!("signing-params response omitted `{field}`"))
}

async fn sign(ctx: &Ctx, args: SignArgs) -> CmdResult {
    let task_id = parse_task_id(&args.task_id)?;
    let client = ctx.client()?;

    // 1. Server-derived params. Nothing below is computed locally.
    let params = match client.inner().get_dispute_signing_params(&task_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(refusal(map_api_error(e).await)),
    };
    let chain_id = params.chain_id.ok_or_else(|| missing("chain_id"))?;
    let chain_id_u64 = u64::try_from(chain_id)
        .map_err(|_| CmdError::Decode(format!("signing-params chain_id={chain_id} is negative")))?;
    let raw_contract = params
        .task_escrow_contract
        .as_deref()
        .ok_or_else(|| missing("task_escrow_contract"))?;
    let raw_escrow_id = params
        .escrow_id
        .as_deref()
        .ok_or_else(|| missing("escrow_id"))?;

    // Guard 3: well-formed target; the zero address is never a real escrow.
    let contract: Address = raw_contract.parse().map_err(|e| {
        CmdError::Decode(format!(
            "server task_escrow_contract `{raw_contract}` not a valid EVM address: {e}"
        ))
    })?;
    if contract == Address::ZERO {
        return Err(CmdError::Decode(
            "server returned the zero address as task_escrow_contract; refusing to sign".into(),
        ));
    }
    let escrow_id = B256::from_str(raw_escrow_id).map_err(|e| {
        CmdError::Decode(format!(
            "server escrow_id `{raw_escrow_id}` not a 0x-prefixed 32-byte hex: {e}"
        ))
    })?;

    // 2. Signer + guards 4 and 1.
    let keystore_ref = args.keystore.as_deref().map(str::to_string).or_else(|| {
        ctx.keystore_path
            .as_deref()
            .and_then(|p| p.to_str().map(str::to_string))
    });
    let signer = super::wallet_args::load_signer(
        keystore_ref.as_deref(),
        args.wallet_password_file.as_deref(),
        "escrow dispute",
    )?;
    if let Some(expected) = args
        .wallet_address
        .as_deref()
        .or(ctx.wallet_address.as_deref())
    {
        let expected_addr: Address = expected.parse().map_err(|e| {
            CmdError::Usage(format!("--wallet-address is not a valid EVM address: {e}"))
        })?;
        if signer.address() != expected_addr {
            return Err(CmdError::Usage(format!(
                "keystore address {:#x} does not match --wallet-address {expected}",
                signer.address()
            )));
        }
    }
    let mut warnings: Vec<SecurityWarning> = Vec::new();
    match params.poster_wallet_address.as_deref() {
        Some(poster) => {
            let poster_addr: Address = poster.parse().map_err(|e| {
                CmdError::Decode(format!(
                    "server poster_wallet_address `{poster}` not a valid EVM address: {e}"
                ))
            })?;
            if poster_addr != signer.address() {
                return Err(CmdError::Usage(format!(
                    "keystore address {:#x} is not the escrow's poster_wallet_address {poster}; \
                     connect the wallet that funded this escrow",
                    signer.address()
                )));
            }
        }
        None => warnings.push(SecurityWarning {
            code: "poster_wallet_unknown",
            message: "server sent no poster_wallet_address (older escrow); the contract still \
                      reverts if this wallet is not the poster"
                .into(),
        }),
    }
    if params.lapse_pending == Some(true) {
        warnings.push(SecurityWarning {
            code: "lapse_pending",
            message: format!(
                "the signing deadline ({}) has passed; the server still accepts a signature \
                 until it records the lapse, which completes the task in the worker's favor",
                params
                    .escalation_deadline
                    .map_or_else(|| "unknown".to_string(), |d| d.to_rfc3339())
            ),
        });
    }

    // 3. Guard 2 (configured network) + RPC URL.
    let cfg = client.fetch_network_config().await.map_err(|e| {
        CmdError::Server(format!("fetch network config from {}: {e}", ctx.base_url()))
    })?;
    let network = ctx.environment.network();
    let entry = cfg.entry(network.as_str()).map_err(|e| {
        CmdError::Server(format!(
            "deployment at {} does not advertise network `{}`: {e}",
            ctx.base_url(),
            network.as_str()
        ))
    })?;
    if entry.chain_id != chain_id {
        return Err(CmdError::Validation {
            code: "chain_id_mismatch".into(),
            message: format!(
                "signing-params chain_id={chain_id} but the configured network `{}` is chain_id={}; refusing to sign",
                network.as_str(),
                entry.chain_id
            ),
        });
    }
    let rpc_url = if let Some(ref url) = args.rpc_url {
        validate_override_rpc_url(
            url,
            network_policy_for_chain_id(chain_id_u64),
            ctx.allow_custom_endpoints,
        )?;
        url.clone()
    } else {
        if !is_proxy_rpc_url(&entry.rpc_url, ctx.base_url()) {
            return Err(CmdError::Server(format!(
                "deployment at {} returned rpc_url {:?} for chain_id={chain_id}, which is not on \
                 the same host. Refusing to route RPC traffic off-host.",
                ctx.base_url(),
                entry.rpc_url,
            )));
        }
        entry.rpc_url.clone()
    };

    let calldata: Bytes = dispute_calldata(escrow_id).into();
    let common = |action: &str| {
        json!({
            "action": action,
            "task_id": task_id.to_string(),
            "escrow_id": format!("{escrow_id:#x}"),
            "to": format!("{contract:#x}"),
            "chain_id": chain_id,
            "from": format!("{:#x}", signer.address()),
            "calldata": format!("0x{}", hex::encode(&calldata)),
            "lapse_pending": params.lapse_pending,
            "escalation_deadline": params.escalation_deadline,
        })
    };

    if ctx.dry_run {
        return Ok(
            Envelope::success(ctx.environment, ctx.dry_run, common("would_submit_dispute"))
                .with_warnings(warnings),
        );
    }

    // 4. Live: guard 2 against the node, then send.
    let http = ctx.rpc_http_client(&client, &rpc_url);
    let rpc = TempoRpcClient::new(http, rpc_url.clone());
    let node_chain = rpc
        .chain_id()
        .await
        .map_err(|e| CmdError::Server(format!("eth_chainId from {rpc_url}: {e}")))?;
    if node_chain != chain_id_u64 {
        return Err(CmdError::Validation {
            code: "chain_id_mismatch".into(),
            message: format!(
                "RPC node reports chain_id={node_chain} but signing-params say {chain_id}; refusing to sign"
            ),
        });
    }
    let receipt_timeout = super::resolve_duration(
        args.receipt_timeout,
        ctx.receipt_timeout,
        default_receipt_timeout(chain_id_u64),
    );
    let tx = sign_and_broadcast_tx(&rpc, &signer, contract, calldata.clone())
        .await
        .map_err(|e| CmdError::Server(format!("dispute broadcast failed: {e}")))?;
    let ok = rpc
        .wait_for_receipt(tx, receipt_timeout, RECEIPT_POLL_INTERVAL)
        .await
        .map_err(|e| CmdError::Server(format!("dispute receipt: {e}")))?;
    if !ok {
        return Err(CmdError::Server(format!(
            "dispute tx {tx:#x} reverted on-chain (Unauthorized: not the poster's wallet; \
             InvalidState: the escrow can no longer be disputed)"
        )));
    }
    let mut data = common("dispute_submitted");
    data["tx_hash"] = json!(format!("{tx:#x}"));
    data["note"] = json!(
        "submitted; the dispute counts as signed once TaskFast observes the finalized \
         EscrowDisputed log"
    );
    Ok(Envelope::success(ctx.environment, ctx.dry_run, data).with_warnings(warnings))
}
