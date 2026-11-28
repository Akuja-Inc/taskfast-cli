// SPDX-License-Identifier: MIT
//! `taskfast fee retry` — re-attempt a failed submission-fee charge.
//!
//! Wraps `GET /tasks/{id}/retry-fee` and `POST /tasks/{id}/retry-fee`.
//! Since 2026-11-01 the server requires the poster's freshly signed fee transfer
//! (a 0x-hex signed ERC-20 transfer, or the hash of one the poster already broadcast)
//! in the request body; the original voucher cannot be reused.
//!
//! When `--submission-fee-voucher` is omitted, the CLI fetches the payment parameters
//! (`GET /tasks/{id}/retry-fee`), validates the network allowlists, verifies that
//! the calldata matches the payment parameters, signs and broadcasts the payment
//! using the poster's keystore wallet under the per-wallet lock (all through the
//! chain-session seam: endpoint guards, node chain id, wallet unlock), and submits
//! the transaction hash to the server.
//!
//! If the broadcast succeeds but the server submission fails, the transaction hash
//! is persisted in `.taskfast/` via `StateStore`. Re-running the command reuses
//! the broadcast transaction hash rather than paying twice.
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
use crate::tempo_session::{ChainConnection, ChainTarget, WalletOpts};

use taskfast_client::api::types::RetrySubmissionFeeRequest;
use taskfast_client::map_api_error;

alloy_sol_types::sol! {
    function transfer(address to, uint256 amount);
    function transferWithMemo(address to, uint256 amount, bytes32 memo_hash);
}

/// Verify that the calldata in `payload_to_sign` encodes an ERC-20 `transfer`
/// or TIP-20 `transferWithMemo` call with recipient, amount, and memo matching
/// the server's payment parameters.
pub fn validate_fee_calldata(
    calldata: &[u8],
    expected_recipient: &str,
    expected_amount: &str,
    expected_memo_hash: Option<&str>,
) -> Result<(), CmdError> {
    use alloy_sol_types::SolCall;

    let expected_to: Address = expected_recipient.parse().map_err(|e| {
        CmdError::Server(format!(
            "server returned invalid recipient `{expected_recipient}`: {e}"
        ))
    })?;
    let expected_amt: alloy_primitives::U256 = expected_amount.parse().map_err(|e| {
        CmdError::Server(format!(
            "server returned invalid amount `{expected_amount}`: {e}"
        ))
    })?;

    if let Ok(call) = transferWithMemoCall::abi_decode(calldata) {
        if call.to != expected_to {
            return Err(CmdError::Validation {
                code: "calldata_mismatch".into(),
                message: format!(
                    "calldata recipient ({:#x}) does not match server recipient ({:#x})",
                    call.to, expected_to
                ),
            });
        }
        if call.amount != expected_amt {
            return Err(CmdError::Validation {
                code: "calldata_mismatch".into(),
                message: format!(
                    "calldata amount ({}) does not match server amount ({})",
                    call.amount, expected_amt
                ),
            });
        }
        if let Some(expected_memo) = expected_memo_hash {
            let expected_memo_b256: alloy_primitives::B256 =
                expected_memo.parse().map_err(|e| {
                    CmdError::Server(format!(
                        "server returned invalid memo_hash `{expected_memo}`: {e}"
                    ))
                })?;
            if call.memo_hash != expected_memo_b256 {
                return Err(CmdError::Validation {
                    code: "calldata_mismatch".into(),
                    message: format!(
                        "calldata memo_hash ({:#x}) does not match server memo_hash ({:#x})",
                        call.memo_hash, expected_memo_b256
                    ),
                });
            }
        }
        return Ok(());
    }

    if let Ok(call) = transferCall::abi_decode(calldata) {
        if call.to != expected_to {
            return Err(CmdError::Validation {
                code: "calldata_mismatch".into(),
                message: format!(
                    "calldata recipient ({:#x}) does not match server recipient ({:#x})",
                    call.to, expected_to
                ),
            });
        }
        if call.amount != expected_amt {
            return Err(CmdError::Validation {
                code: "calldata_mismatch".into(),
                message: format!(
                    "calldata amount ({}) does not match server amount ({})",
                    call.amount, expected_amt
                ),
            });
        }
        return Ok(());
    }

    Err(CmdError::Validation {
        code: "invalid_calldata".into(),
        message: "payload_to_sign is not a valid ERC-20 transfer or TIP-20 transferWithMemo call"
            .into(),
    })
}

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
    let state_store = ctx.state_store();

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
            Ok(v) => {
                let _ = state_store.clear_pending_fee_retry(&task_id);
                v.into_inner()
            }
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

    // Pass-through form was not provided. Check whether a previous broadcast
    // was recorded in local state, avoiding double-payment if server submit failed.
    let client = ctx.client()?;
    let pending_voucher = state_store.get_pending_fee_retry(&task_id);

    let mut voucher_saved = true;
    let voucher_to_submit = if let Some(saved_tx_hash) = pending_voucher {
        eprintln!(
            "taskfast: found pending broadcast transaction {saved_tx_hash} for task {task_id}; \
             reusing to avoid double-payment"
        );
        saved_tx_hash
    } else {
        // Read payment data from server. Server's GET /retry-fee guards status
        // and returns 409 retry_not_needed / invalid_status when not retryable.
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
                    "token_address": payment.token_address,
                    "recipient": payment.recipient,
                    "amount": payment.amount,
                    "memo_text": payment.memo_text,
                    "memo_hash": payment.memo_hash,
                }),
            ));
        }

        state_store
            .preflight(crate::state_store::StateAsset::FeeRetries)
            .map_err(|e| {
                CmdError::Usage(format!(
                    "cannot save the pending fee retry before broadcasting: {e}"
                ))
            })?;

        let token_address_str = payment
            .token_address
            .as_deref()
            .ok_or_else(|| CmdError::Server("server returned no token_address".into()))?;

        let payload_str = payment
            .payload_to_sign
            .as_deref()
            .ok_or_else(|| CmdError::Server("server returned no payload_to_sign".into()))?;
        let calldata = super::wallet_args::decode_0x_bytes(payload_str).map_err(|e| {
            CmdError::Server(format!("server returned invalid payload_to_sign: {e}"))
        })?;

        let recipient_str = payment
            .recipient
            .as_deref()
            .ok_or_else(|| CmdError::Server("server returned no recipient".into()))?;
        let amount_str = payment
            .amount
            .as_deref()
            .ok_or_else(|| CmdError::Server("server returned no amount".into()))?;

        validate_fee_calldata(
            &calldata,
            recipient_str,
            amount_str,
            payment.memo_hash.as_ref().map(|s| s.as_str()),
        )?;

        // Endpoint guards + node chain id + wallet unlock, behind the seam.
        // No server-issued chain id here, so the node's own chain id is what
        // the fee guards key on.
        let wallet = WalletOpts {
            keystore: args.keystore.as_deref(),
            password_file: args.wallet_password_file.as_deref(),
            wallet_address: args.wallet_address.as_deref(),
            rpc_url: args.rpc_url.as_deref(),
            receipt_timeout: None,
        };
        let session = ChainConnection::resolve(ctx, &client, wallet.rpc_url, ChainTarget::Node)
            .await?
            .into_session(ctx, &wallet, "submission fee")
            .await?;
        let tx_hash = session
            .send_fee_transfer(
                ctx,
                token_address_str,
                alloy_primitives::Bytes::from(calldata),
            )
            .await?;
        let tx_hash_hex = format!("{tx_hash:#x}");

        voucher_saved = !state_store.is_ephemeral();
        if let Err(e) = state_store.record_pending_fee_retry(&task_id, &tx_hash_hex) {
            voucher_saved = false;
            eprintln!("taskfast: warning: failed to persist pending fee retry to state store: {e}");
        }

        tx_hash_hex
    };

    let body = RetrySubmissionFeeRequest {
        submission_fee_voucher: voucher_to_submit
            .parse()
            .map_err(|e| CmdError::Server(format!("tx hash rejected by schema pattern: {e}")))?,
    };

    let resp = match client.inner().retry_submission_fee(&task_id, &body).await {
        Ok(v) => {
            let _ = state_store.clear_pending_fee_retry(&task_id);
            v.into_inner()
        }
        Err(e) => {
            let server_err = map_api_error(e).await;
            let mut retry_not_needed = false;
            if let taskfast_client::Error::Validation { code, .. } = &server_err {
                if code == "retry_not_needed" {
                    let _ = state_store.clear_pending_fee_retry(&task_id);
                    retry_not_needed = true;
                }
            }
            if !retry_not_needed {
                eprintln!(
                    "taskfast: fee transfer was broadcast ({voucher_to_submit}) but server retry failed: {server_err}.\n{}",
                    retry_guidance(&task_id.to_string(), &voucher_to_submit, voucher_saved)
                );
            }
            return Err(server_err.into());
        }
    };

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": resp.task_id,
            "status": resp.status,
            "message": resp.message,
            "submission_fee_voucher": voucher_to_submit,
        }),
    ))
}

fn retry_guidance(task_id: &str, voucher: &str, voucher_saved: bool) -> String {
    let explicit = format!("taskfast fee retry {task_id} --submission-fee-voucher {voucher}");
    if voucher_saved {
        format!(
            "The pending transaction has been saved locally; re-running `taskfast fee retry {task_id}` will reuse it.\nAlternatively, run:\n  {explicit}"
        )
    } else {
        format!(
            "The pending transaction was NOT saved locally; do not re-run a plain `fee retry` (it would pay again). Retry only with:\n  {explicit}"
        )
    }
}

#[cfg(test)]
mod guidance_tests {
    use super::retry_guidance;

    #[test]
    fn saved_voucher_allows_plain_retry() {
        let g = retry_guidance("t1", "0xabc", true);
        assert!(g.contains("will reuse it"));
        assert!(g.contains("--submission-fee-voucher 0xabc"));
    }

    #[test]
    fn unsaved_voucher_forbids_plain_retry() {
        let g = retry_guidance("t1", "0xabc", false);
        assert!(g.contains("do not re-run a plain"));
        assert!(!g.contains("will reuse it"));
        assert!(g.contains("--submission-fee-voucher 0xabc"));
    }
}
