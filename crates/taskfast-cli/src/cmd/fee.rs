// SPDX-License-Identifier: MIT
//! `taskfast fee retry` — re-attempt a failed submission-fee charge.
//!
//! Wraps `GET /tasks/{id}/retry-fee` and `POST /tasks/{id}/retry-fee`.
//! Since 2026-11-01 the server requires the poster's freshly signed fee transfer
//! (a 0x-hex signed ERC-20 transfer, or the hash of one the poster already broadcast)
//! in the request body; the original voucher cannot be reused.
//!
//! When `--submission-fee-voucher` is omitted, the CLI fetches the payment parameters
//! (`GET /tasks/{id}/retry-fee`), validates the network allowlists, signs and broadcasts
//! the payment using the poster's keystore wallet under the per-wallet lock, and submits
//! the transaction hash to the server.
//!
//! When `--submission-fee-voucher` is passed, the CLI passes the value through unchanged.
//! 409 `retry_not_needed` / `retry_in_progress` / `fee_executed_unmatched` surface with
//! their stable codes.

use std::path::PathBuf;

use alloy_primitives::Address;
use clap::Parser;
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_agent::tempo_rpc::TempoRpcClient;
use taskfast_chains::tempo::{is_allowed_fee_token, is_known_network};
use taskfast_client::api::types::RetrySubmissionFeeRequest;
use taskfast_client::map_api_error;

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Poster: re-attempt the submission-fee charge with a freshly signed fee transfer.
    Retry(RetryArgs),
}

#[derive(Debug, Parser)]
pub struct RetryArgs {
    /// Task UUID (parked at `blocked_on_submission_fee_debt`).
    pub task_id: String,

    /// The poster's freshly signed submission-fee transfer (0x-hex), or the
    /// hash of one already broadcast. The original voucher cannot be reused.
    /// When omitted, the CLI builds, signs and sends the replacement fee
    /// payment with the poster's wallet.
    #[arg(long)]
    pub submission_fee_voucher: Option<String>,

    /// Poster wallet address (0x-prefixed). Matches `TEMPO_WALLET_ADDRESS`.
    #[arg(long, env = "TEMPO_WALLET_ADDRESS")]
    pub wallet_address: Option<String>,

    /// Keystore reference. Accepts a bare path or `file:/path`.
    #[arg(long, env = "TEMPO_KEY_SOURCE")]
    pub keystore: Option<String>,

    /// Path to a file holding the keystore password.
    #[arg(long, env = "TASKFAST_WALLET_PASSWORD_FILE")]
    pub wallet_password_file: Option<PathBuf>,

    /// Tempo RPC endpoint override.
    #[arg(long, env = "TEMPO_RPC_URL")]
    pub rpc_url: Option<String>,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Retry(a) => retry(ctx, a).await,
    }
}

async fn retry(ctx: &Ctx, args: RetryArgs) -> CmdResult {
    let task_id = Uuid::parse_str(&args.task_id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;

    if let Some(voucher) = args.submission_fee_voucher {
        if voucher.trim().is_empty() {
            return Err(CmdError::Usage(
                "--submission-fee-voucher is empty; the server requires the poster's freshly signed \
                 fee transfer (or the hash of one the poster broadcast). The original voucher \
                 cannot be reused."
                    .into(),
            ));
        }
        if ctx.dry_run {
            return Ok(Envelope::success(
                ctx.environment,
                ctx.dry_run,
                json!({
                    "action": "would_retry_fee",
                    "task_id": task_id.to_string(),
                    "submission_fee_voucher": voucher,
                }),
            ));
        }
        let client = ctx.client()?;
        let body = RetrySubmissionFeeRequest {
            submission_fee_voucher: voucher.parse().map_err(|e| {
                CmdError::Usage(format!(
                    "--submission-fee-voucher is not 0x-prefixed hex (the server would answer \
                     400 invalid_submission_fee_voucher): {e}"
                ))
            })?,
        };
        let resp = match client.inner().retry_submission_fee(&task_id, &body).await {
            Ok(v) => v.into_inner(),
            Err(e) => return Err(map_api_error(e).await.into()),
        };
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "task_id": resp.task_id,
                "status": resp.status,
                "message": resp.message,
            }),
        ));
    }

    // Pass-through form was not provided. Build, sign and send replacement fee payment.
    let client = ctx.client()?;

    // Guard: command sends nothing unless the task shows next_action: retry_submission_fee.
    let task = match client.inner().get_task(&task_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };

    if task.next_action.as_deref() != Some("retry_submission_fee") {
        let status_str = task.status.as_deref().unwrap_or("unknown");
        let message = task
            .message
            .or(task.blocked_reason)
            .unwrap_or_else(|| format!("task status `{status_str}` is not awaiting fee retry"));
        return Err(CmdError::Validation {
            code: "retry_not_needed".into(),
            message,
        });
    }

    // Read payment data from server.
    let payment = match client
        .inner()
        .get_submission_fee_retry_payment(&task_id)
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_retry_fee",
                "task_id": task_id.to_string(),
                "token": payment.token_address,
                "token_address": payment.token_address,
                "recipient": payment.recipient,
                "amount": payment.amount,
                "memo": payment.memo_hash,
                "memo_text": payment.memo_text,
                "memo_hash": payment.memo_hash,
            }),
        ));
    }

    let token_address_str = payment
        .token_address
        .as_deref()
        .ok_or_else(|| CmdError::Server("server returned no token_address".into()))?;
    let token_address: Address = token_address_str.parse().map_err(|e| {
        CmdError::Server(format!(
            "server returned invalid token_address `{token_address_str}`: {e}"
        ))
    })?;

    let payload_str = payment
        .payload_to_sign
        .as_deref()
        .ok_or_else(|| CmdError::Server("server returned no payload_to_sign".into()))?;
    let calldata = super::post::decode_0x_bytes(payload_str)
        .map_err(|e| CmdError::Server(format!("server returned invalid payload_to_sign: {e}")))?;

    let rpc_url = match args.rpc_url {
        Some(override_url) => {
            super::validate_override_rpc_url(
                &override_url,
                ctx.environment.network(),
                ctx.allow_custom_endpoints,
            )?;
            override_url
        }
        None => super::post::resolve_proxy_rpc_url(&client, ctx).await?,
    };

    let keystore_path = super::wallet_args::resolve_keystore_path(
        args.keystore.as_deref(),
        ctx.keystore_path.as_deref(),
    );
    let signer = super::wallet_args::load_signer_and_validate(
        keystore_path.as_deref(),
        args.wallet_password_file.as_deref(),
        args.wallet_address
            .as_deref()
            .or(ctx.wallet_address.as_deref()),
        "submission fee",
    )?;

    let http = ctx.rpc_http_client(&client, &rpc_url);
    let rpc = TempoRpcClient::new(http, rpc_url.clone());

    // Guard: chain id check against node.
    let chain_id = rpc
        .chain_id()
        .await
        .map_err(|e| CmdError::Server(format!("tempo rpc eth_chainId: {e}")))?;
    if !ctx.allow_custom_endpoints && !is_known_network(chain_id) {
        return Err(CmdError::Validation {
            code: "unknown_chain".into(),
            message: format!(
                "RPC reports chain_id={chain_id} which is not a known Tempo or \
                 local-dev network; refusing to sign. Pass --allow-custom-endpoints \
                 to override."
            ),
        });
    }

    // Guard: fee token allowlist check.
    if !is_allowed_fee_token(chain_id, token_address_str) {
        return Err(CmdError::Validation {
            code: "fee_token_not_allowed".into(),
            message: format!(
                "server returned token_address {token_address_str} which is not on the PathUSD \
                 allowlist for chain_id={chain_id}. Refusing to sign — this would \
                 redirect your fee transfer to an arbitrary ERC-20 contract."
            ),
        });
    }

    eprintln!(
        "taskfast: signing submission-fee transfer — token={} chain_id={} rpc={}",
        token_address_str, chain_id, rpc_url
    );

    // Guard: per-wallet lock around nonce via TempoSession.
    let session = crate::tempo_session::TempoSession::new(
        signer,
        keystore_path,
        rpc,
        std::time::Duration::from_mins(1),
    );

    let tx_hash = session
        .send_transaction(token_address, alloy_primitives::Bytes::from(calldata))
        .await?;
    let tx_hash_hex = format!("{tx_hash:#x}");

    let body = RetrySubmissionFeeRequest {
        submission_fee_voucher: tx_hash_hex
            .parse()
            .map_err(|e| CmdError::Server(format!("tx hash rejected by schema pattern: {e}")))?,
    };

    let resp = match client.inner().retry_submission_fee(&task_id, &body).await {
        Ok(v) => v.into_inner(),
        Err(e) => {
            let server_err = map_api_error(e).await;
            let retry_cmd =
                format!("taskfast fee retry {task_id} --submission-fee-voucher {tx_hash_hex}");
            eprintln!(
                "taskfast: fee transfer was broadcast ({tx_hash_hex}) but server retry failed: {server_err}.\n\
                 To retry without paying again, run:\n  {retry_cmd}"
            );
            return Err(CmdError::Server(format!(
                "fee transfer was broadcast ({tx_hash_hex}) but server retry failed ({server_err}); \
                 do not pay again. Re-run with the existing voucher: {retry_cmd}"
            )));
        }
    };

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": resp.task_id,
            "status": resp.status,
            "message": resp.message,
            "submission_fee_voucher": tx_hash_hex,
        }),
    ))
}
