// SPDX-License-Identifier: MIT
//! `taskfast escrow sign <bid_id>` — headless poster escrow signing.
//!
//! Replaces the web-UI-only wagmi + passkey path at `assets/js/escrow_sign.js`.
//! Picks up bids the server parked in `:accepted_pending_escrow` after
//! `taskfast bid accept` and drives them to `:accepted` without a browser:
//!
//!  1. `GET /bids/:id/escrow/params` → server-derived on-chain params.
//!  2. EIP-712 `DistributionDomain` from those params (chain_id +
//!     task_escrow_contract) — the venue's escrow for zone-bound tasks.
//!  3. ERC-20 `allowance` preflight; `approve` if short.
//!  4. Random 32-byte salt → `compute_escrow_id` (matches Solidity).
//!  5. EIP-712 `DistributionApproval(escrowId, deadline)` — `sign_distribution`.
//!  6. `TaskEscrow.open` / `openWithMemo` — broadcast + wait for receipt.
//!  7. `POST /bids/:id/escrow/finalize` with voucher + sig + deadline.
//!
//! Memo is server-driven: `memo_hash` in the params payload selects
//! `openWithMemo` vs `open`. No `--memo` flag.
//!
//! Error mapping delegates to `map_api_error` (401|403→Auth, 409|422→
//! Validation). Keystore / signing failures surface as `Wallet` (exit 5).

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use clap::Parser;
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_agent::chain::{compute_escrow_id, EscrowIdParams, TaskEscrow};
use taskfast_agent::tempo_rpc::TempoRpcClient;
use taskfast_chains::tempo::{sign_distribution, DistributionDomain, SigningError};
use taskfast_client::api::types::{
    BidEscrowFinalizeRequest, BidEscrowFinalizeRequestPosterApprovalDeadline,
    BidEscrowParamsResponse,
};
use taskfast_client::map_api_error;

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Sign + broadcast the on-chain escrow for a deferred bid; POST finalize.
    Sign(SignArgs),
}

#[derive(Debug, Parser)]
pub struct SignArgs {
    /// Bid UUID. Bid must be in `:accepted_pending_escrow`; caller must be
    /// the parent task's poster.
    pub bid_id: String,

    /// Keystore reference (same form as `taskfast post` / `settle`).
    #[arg(long, env = "TEMPO_KEY_SOURCE")]
    pub keystore: Option<String>,

    /// Path to keystore password file.
    #[arg(long, env = "TASKFAST_WALLET_PASSWORD_FILE")]
    pub wallet_password_file: Option<PathBuf>,

    /// Poster wallet address preflight (0x-prefixed). When set, we fail
    /// before touching the chain if the keystore decrypts to a mismatch.
    #[arg(long, env = "TEMPO_WALLET_ADDRESS")]
    pub wallet_address: Option<String>,

    /// Tempo RPC override. Defaults to the deployment's authenticated
    /// proxy URL for `params.chain_id` (reverse-looked-up in the
    /// `GET /config/network` map returned by the backend).
    #[arg(long, env = "TEMPO_RPC_URL")]
    pub rpc_url: Option<String>,

    /// Skip the on-chain `allowance` preflight + `approve` tx. Only safe when
    /// the caller already granted a sufficient allowance out-of-band.
    #[arg(long)]
    pub skip_allowance_check: bool,

    /// Receipt-polling ceiling — human duration (`3min`, `90s`). Falls
    /// back to `receipt_timeout` in config, then a chain-aware default
    /// (3min mainnet, 1min testnet).
    #[arg(
        long,
        env = "TASKFAST_RECEIPT_TIMEOUT",
        value_parser = humantime::parse_duration,
    )]
    pub receipt_timeout: Option<Duration>,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Sign(a) => sign(ctx, a).await,
    }
}

async fn sign(ctx: &Ctx, args: SignArgs) -> CmdResult {
    // 1. Validate bid UUID locally.
    let bid_id = Uuid::parse_str(&args.bid_id)
        .map_err(|e| CmdError::Usage(format!("bid id must be a UUID: {e}")))?;

    let client = ctx.client()?;

    // 2. Fetch escrow params — server enforces poster-auth + bid status.
    let params = match client.inner().get_bid_escrow_params(&bid_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => {
            let mapped = map_api_error(e).await;
            // Version-skew hint: a server predating the v2 arbitrated-escrow
            // rollout omits `arbitrator_address`, which this CLI now requires,
            // so the 200 body fails to deserialize into a generic serde
            // "missing field" decode error. Surface something actionable.
            // ponytail: substring-match the stable serde message; revisit if
            // the generated field is ever renamed.
            if matches!(&mapped, taskfast_client::Error::Decode(_))
                && mapped.to_string().contains("arbitrator_address")
            {
                return Err(CmdError::Validation {
                    code: "server_missing_arbitrator".into(),
                    message: "server omitted `arbitrator_address`; this deployment predates \
                              v2 arbitrated escrow — upgrade the server or use a CLI version \
                              that matches it"
                        .into(),
                });
            }
            return Err(mapped.into());
        }
    };

    // 3. The EIP-712 signing domain comes from the escrow params themselves
    //    (chain_id + task_escrow_contract), built below once task_escrow is
    //    parsed. For a zone-bound task that is the venue's escrow — the
    //    contract that verifies the DistributionApproval. Sourcing the domain
    //    from the global `GET /agents/me/readiness` settlement_domain instead
    //    hard-failed zone tasks whose escrow lives off the L1 contract (gh#111).

    // 4. Parse addresses + scale decimal amounts to U256 raw units.
    let token_address: Address = params.token_address.parse().map_err(|e| {
        CmdError::Decode(format!(
            "server token_address `{}` not a valid EVM address: {e}",
            params.token_address
        ))
    })?;
    let task_escrow: Address = params.task_escrow_contract.parse().map_err(|e| {
        CmdError::Decode(format!(
            "server task_escrow_contract `{}` not a valid EVM address: {e}",
            params.task_escrow_contract
        ))
    })?;
    let worker: Address = params.worker_address.parse().map_err(|e| {
        CmdError::Decode(format!(
            "server worker_address `{}` not a valid EVM address: {e}",
            params.worker_address
        ))
    })?;
    let platform_wallet: Address = params.platform_wallet.parse().map_err(|e| {
        CmdError::Decode(format!(
            "server platform_wallet `{}` not a valid EVM address: {e}",
            params.platform_wallet
        ))
    })?;
    // Canonical v2 ArbitratedEscrow binds the arbitrator into open/openWithMemo,
    // the escrow-id preimage, and EscrowOpened. Omitting it reverts the open on
    // the v2 contract and breaks server-side arbitrator validation (gh#674).
    let arbitrator: Address = params.arbitrator_address.parse().map_err(|e| {
        CmdError::Decode(format!(
            "server arbitrator_address `{}` not a valid EVM address: {e}",
            params.arbitrator_address
        ))
    })?;
    // A zero arbitrator is never valid on v2: `open` reverts with
    // arbitrator_not_in_pool. Reject it client-side before signing/broadcast
    // rather than burning the approve tx's gas on a doomed open (gh#674).
    if arbitrator == Address::ZERO {
        return Err(CmdError::Decode(
            "server returned a zero arbitrator_address; the v2 escrow open requires a \
             pool arbitrator and would revert (arbitrator_not_in_pool)"
                .to_string(),
        ));
    }

    // Signing domain: the escrow that will verify the DistributionApproval is
    // task_escrow_contract itself (the venue's escrow for a zone-bound task),
    // so it is the EIP-712 verifying contract. chain_id likewise comes from the
    // per-task params, not the global readiness domain (gh#111).
    let chain_id_u64 = u64::try_from(params.chain_id).map_err(|_| {
        CmdError::Decode(format!(
            "escrow params chain_id={} is negative",
            params.chain_id
        ))
    })?;
    let verifying_contract = task_escrow;
    let domain = DistributionDomain::new(chain_id_u64, verifying_contract);

    let decimals = u8::try_from(params.decimals).map_err(|_| {
        CmdError::Decode(format!(
            "escrow params decimals={} out of u8 range",
            params.decimals
        ))
    })?;
    let deposit = decimal_to_u256(&params.amount, decimals)?;
    let platform_fee = decimal_to_u256(&params.platform_fee_amount, decimals)?;

    // 5. Optional memo — pass through exactly; server re-derives authoritative
    //    memo on finalize, so a client mismatch is a Validation error there.
    let memo_hash_opt: Option<B256> = params
        .memo_hash
        .as_deref()
        .map(|s| {
            B256::from_str(s).map_err(|e| {
                CmdError::Decode(format!(
                    "server memo_hash `{s}` not a 0x-prefixed 32-byte hex: {e}"
                ))
            })
        })
        .transpose()?;

    // 6. Load signer + preflight address equality.
    let keystore_ref = super::wallet_args::resolve_keystore_ref(
        args.keystore.as_deref(),
        ctx.keystore_path.as_deref(),
    );
    let expected_wallet = super::wallet_args::resolve_expected_wallet_address(
        args.wallet_address.as_deref(),
        ctx.wallet_address.as_deref(),
    );
    let signer = super::wallet_args::load_signer_and_validate(
        keystore_ref.as_deref(),
        args.wallet_password_file.as_deref(),
        expected_wallet,
        "escrow approval",
    )?;

    // 7. Derive random salt + predicted escrow_id. Must byte-match Solidity
    //    `TaskEscrow.computeEscrowId`. See
    //    `taskfast-agent::chain::compute_escrow_id` for the caveat on
    //    fee-on-transfer tokens.
    let salt = B256::from(rand::random::<[u8; 32]>());
    let escrow_id = compute_escrow_id(&EscrowIdParams {
        poster: signer.address(),
        worker,
        token: token_address,
        deposit,
        platform_fee_amount: platform_fee,
        platform: platform_wallet,
        arbitrator,
        salt,
    });

    // 8. Sign DistributionApproval(escrowId, deadline). The deadline is the
    //    server-issued `poster_approval_deadline` (absolute Unix seconds), signed
    //    verbatim — the CLI is a dumb orchestrator and does not own this policy
    //    value. Defense-in-depth: if the just-fetched deadline already has less
    //    than `poster_approval_min_lifetime` remaining, abort *before* any tx so
    //    we never strand funds behind an approval the server would reject at
    //    finalize (gh#118 / taskfast#935).
    let mut deadline_unix = server_deadline_or_abort(&params)?;
    let mut signature_hex = sign_deadline(&signer, &domain, escrow_id, deadline_unix)?;

    // 9. Build open / openWithMemo calldata up front — we reuse it for the
    //    dry-run envelope and the live broadcast.
    let open_calldata: Bytes = if let Some(memo_hash) = memo_hash_opt {
        TaskEscrow::openWithMemoCall {
            token: token_address,
            deposit,
            worker,
            platformFeeAmount: platform_fee,
            platform: platform_wallet,
            arbitrator,
            salt,
            memoHash: memo_hash,
        }
        .abi_encode()
        .into()
    } else {
        TaskEscrow::openCall {
            token: token_address,
            deposit,
            worker,
            platformFeeAmount: platform_fee,
            platform: platform_wallet,
            arbitrator,
            salt,
        }
        .abi_encode()
        .into()
    };

    // Resolve RPC URL. Override flows to a bare upstream gateway; default
    // path pulls the deployment's proxy URL from `GET /config/network`
    // (reverse-lookup by chain_id, so the selected entry matches the
    // server-returned escrow params). The proxy URL is sanity-checked to
    // live under the authenticated api_base — catches a misconfigured (or
    // malicious) deployment returning an off-host upstream.
    let (rpc_url, http) = crate::tempo_session::resolve_rpc_endpoint(
        ctx,
        args.rpc_url.as_deref(),
        Some(params.chain_id),
    )
    .await?;

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_sign_escrow",
                "bid_id": bid_id.to_string(),
                "task_id": params.task_id.to_string(),
                "escrow_id": format!("{escrow_id:#x}"),
                "arbitrator": format!("{arbitrator:#x}"),
                "salt": format!("{salt:#x}"),
                "deadline": deadline_unix,
                "signature": signature_hex,
                "open_calldata": format!("0x{}", hex::encode(&open_calldata)),
                "memo_hash": params.memo_hash,
                "rpc_url": rpc_url,
                "domain": {
                    "chain_id": chain_id_u64,
                    "verifying_contract": format!("{verifying_contract:#x}"),
                },
            }),
        ));
    }

    let keystore_path = super::wallet_args::resolve_keystore_path(
        args.keystore.as_deref(),
        ctx.keystore_path.as_deref(),
    );
    let rpc = TempoRpcClient::new(http, rpc_url);
    let default_timeout = crate::tempo_session::default_receipt_timeout_for_chain_id(chain_id_u64);
    let receipt_timeout =
        super::resolve_duration(args.receipt_timeout, ctx.receipt_timeout, default_timeout);
    let session = crate::tempo_session::TempoSession::new(
        signer,
        keystore_path,
        rpc,
        chain_id_u64,
        receipt_timeout,
        crate::tempo_session::RECEIPT_POLL_INTERVAL,
    );

    // 10. Live RPC: allowance preflight + optional approve.
    //     TaskEscrow.open calls transferFrom for `deposit + platformFeeAmount` in a
    //     single call, so balance/allowance/approve must cover the sum — not
    //     just deposit.
    let total_required = deposit
        .checked_add(platform_fee)
        .ok_or_else(|| CmdError::Decode("deposit + platform_fee overflow U256".into()))?;

    let mut approval_tx_hex: Option<String> = None;
    if !args.skip_allowance_check {
        let balance = session
            .erc20_balance_of(token_address, session.address())
            .await?;
        if balance < total_required {
            return Err(CmdError::Usage(format!(
                "poster balance {balance} < required {total_required} (deposit {deposit} + fee {platform_fee}) on token {token_address:#x}"
            )));
        }
        approval_tx_hex = session
            .ensure_allowance(token_address, task_escrow, total_required)
            .await?
            .map(|h| format!("{h:#x}"));
    }

    // 11. Broadcast TaskEscrow.open / openWithMemo, wait for receipt.
    let voucher_tx = session
        .send_transaction_and_wait(
            task_escrow,
            open_calldata,
            "open",
            Some(" — finalize aborted"),
        )
        .await?;
    let voucher_hex = format!("{voucher_tx:#x}");

    // 12. POST finalize. The escrow is already funded on-chain, so the voucher
    //     (open tx hash / escrowId) and salt are fixed. If the server rejects the
    //     signed deadline as `deadline_below_minimum` (a fetch gone stale between
    //     params and finalize), re-fetch params and re-sign *the same voucher*
    //     with the fresh deadline — a cheap re-sign, no new tx, no orphaned
    //     escrow. Bounded to one retry; the pre-broadcast abort makes it rare.
    let mut attempts = 0u8;
    let resp = loop {
        // Checked back to i64 at the API boundary — `deadline_unix` originates as
        // the i64 `poster_approval_deadline`, so this always fits, but an explicit
        // conversion refuses to silently wrap if the generated type ever widens.
        let deadline_i64 = deadline_unix
            .map(|d| {
                i64::try_from(d)
                    .map_err(|_| CmdError::Decode(format!("deadline {d} exceeds i64 range")))
            })
            .transpose()?;
        let body = BidEscrowFinalizeRequest {
            voucher: voucher_hex.clone(),
            poster_approval_signature: signature_hex.clone(),
            poster_approval_deadline: deadline_i64
                .map(BidEscrowFinalizeRequestPosterApprovalDeadline::Integer),
            memo_hash: params.memo_hash.clone(),
        };
        match client.inner().finalize_bid_escrow(&bid_id, &body).await {
            Ok(r) => break r.into_inner(),
            Err(e) => {
                let mapped = map_api_error(e).await;
                if attempts == 0 && is_deadline_below_minimum(&mapped) {
                    attempts += 1;
                    let fresh = match client.inner().get_bid_escrow_params(&bid_id).await {
                        Ok(v) => v.into_inner(),
                        Err(e) => return Err(map_api_error(e).await.into()),
                    };
                    deadline_unix = server_deadline_or_abort(&fresh)?;
                    signature_hex =
                        sign_deadline(session.signer(), &domain, escrow_id, deadline_unix)?;
                    continue;
                }
                return Err(mapped.into());
            }
        }
    };

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "bid_id": resp.bid_id,
            "task_id": resp.task_id,
            "status": resp.status,
            "task_status": resp.task_status,
            "escrow_id": format!("{escrow_id:#x}"),
            "voucher_tx_hash": voucher_hex,
            "approval_tx_hash": approval_tx_hex,
            "deadline": deadline_unix,
        }),
    ))
}

/// Return the server-issued `poster_approval_deadline` (Unix seconds) after the
/// defense-in-depth floor check: if it already carries less than
/// `poster_approval_min_lifetime` remaining, abort so no tx is ever broadcast
/// against an approval the server would reject at finalize (gh#118). This should
/// never fire when params are fetched immediately before signing — it catches a
/// stale fetch, clock skew, or a retry.
///
/// Both fields absent with `contract_version == "v3"` is valid (v3 signs its
/// approval later) and yields `None`. Any other absence aborts, as before the
/// fields became optional (taskfast#2044).
fn server_deadline_or_abort(params: &BidEscrowParamsResponse) -> Result<Option<u64>, CmdError> {
    let (deadline, min_lifetime) = match (
        params.poster_approval_deadline,
        params.poster_approval_min_lifetime,
    ) {
        (Some(d), Some(m)) => (d, m),
        (None, None) if params.contract_version.as_deref() == Some("v3") => return Ok(None),
        _ => {
            return Err(CmdError::Validation {
                code: "server_missing_approval_deadline".into(),
                message: "server omitted `poster_approval_deadline`; this deployment predates \
                          the server-issued escrow deadline — upgrade the server or use a CLI \
                          version that matches it"
                    .into(),
            })
        }
    };
    let deadline = u64::try_from(deadline).map_err(|_| {
        CmdError::Decode(format!(
            "server poster_approval_deadline={deadline} is negative"
        ))
    })?;
    let min_lifetime = u64::try_from(min_lifetime).map_err(|_| {
        CmdError::Decode(format!(
            "server poster_approval_min_lifetime={min_lifetime} is negative"
        ))
    })?;
    let now = u64::try_from(chrono::Utc::now().timestamp())
        .map_err(|_| CmdError::Decode("system clock before epoch".into()))?;
    let remaining = deadline.saturating_sub(now);
    if remaining < min_lifetime {
        return Err(CmdError::Validation {
            code: "deadline_below_minimum".into(),
            message: format!(
                "server-issued deadline has {remaining}s remaining, below the \
                 {min_lifetime}s minimum — aborting rather than sign an approval the \
                 server will reject. Re-run once the server re-issues a deadline."
            ),
        });
    }
    Ok(Some(deadline))
}

/// Sign the poster approval when the server issued a deadline; `None` skips it.
fn sign_deadline(
    signer: &PrivateKeySigner,
    domain: &DistributionDomain,
    escrow_id: B256,
    deadline: Option<u64>,
) -> Result<Option<String>, SigningError> {
    deadline
        .map(|d| sign_distribution(signer, domain, escrow_id, U256::from(d)))
        .transpose()
}

/// True when the API surfaced the server's `deadline_below_minimum` finalize
/// rejection, so the caller can re-fetch params + re-sign rather than fail hard.
fn is_deadline_below_minimum(err: &taskfast_client::Error) -> bool {
    matches!(
        err,
        taskfast_client::Error::Validation { code, .. } if code == "deadline_below_minimum"
    )
}

/// Scale a decimal string (`"75.00"`) to raw U256 token units given `decimals`.
/// Rejects negatives, exponential notation, and fractional digits that exceed
/// `decimals` (would silently truncate user intent).
fn decimal_to_u256(s: &str, decimals: u8) -> Result<U256, CmdError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(CmdError::Decode("empty decimal amount".into()));
    }
    if trimmed.starts_with('-') {
        return Err(CmdError::Decode(format!(
            "negative amount `{s}` disallowed"
        )));
    }
    let (whole, frac) = match trimmed.split_once('.') {
        Some((w, f)) => (w, f),
        None => (trimmed, ""),
    };
    let frac_len = frac.len();
    if frac_len > decimals as usize {
        return Err(CmdError::Decode(format!(
            "amount `{s}` has {frac_len} fractional digits but token has only {decimals}"
        )));
    }
    let mut combined = String::with_capacity(whole.len() + decimals as usize);
    combined.push_str(whole);
    combined.push_str(frac);
    for _ in 0..(decimals as usize - frac_len) {
        combined.push('0');
    }
    // Strip leading zeros but keep at least one digit so "0" parses.
    let stripped = combined.trim_start_matches('0');
    let digits = if stripped.is_empty() { "0" } else { stripped };
    U256::from_str_radix(digits, 10)
        .map_err(|e| CmdError::Decode(format!("amount `{s}` not parseable as integer: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use taskfast_chains::tempo::TEMPO_MAINNET_CHAIN_ID;

    fn default_receipt_timeout(chain_id: i64) -> Duration {
        match u64::try_from(chain_id) {
            Ok(id) => crate::tempo_session::default_receipt_timeout_for_chain_id(id),
            _ => crate::tempo_session::DEFAULT_RECEIPT_TIMEOUT_TESTNET,
        }
    }

    #[test]
    fn decimal_to_u256_scales_basic() {
        assert_eq!(
            decimal_to_u256("75.00", 6).unwrap(),
            U256::from(75_000_000u64)
        );
        assert_eq!(decimal_to_u256("75", 6).unwrap(), U256::from(75_000_000u64));
        assert_eq!(decimal_to_u256("0.5", 6).unwrap(), U256::from(500_000u64));
        assert_eq!(decimal_to_u256("0", 6).unwrap(), U256::ZERO);
    }

    #[test]
    fn decimal_to_u256_rejects_excess_fractional_digits() {
        let err = decimal_to_u256("1.1234567", 6).expect_err("7 > 6 must fail");
        matches!(err, CmdError::Decode(_));
    }

    #[test]
    fn decimal_to_u256_rejects_negative() {
        let err = decimal_to_u256("-1.00", 6).expect_err("negative must fail");
        matches!(err, CmdError::Decode(_));
    }

    #[test]
    fn chain_id_constants_match_tempo_protocol() {
        use taskfast_chains::tempo::TEMPO_TESTNET_CHAIN_ID;
        // Pinned against server truth (`lib/task_fast/payments/tempo_constants.ex`)
        // and the `DistributionDomain` domain-separator consumer. Drift here
        // would desync the EIP-712 signing domain from the on-chain verifier.
        assert_eq!(TEMPO_MAINNET_CHAIN_ID, 4_217);
        assert_eq!(TEMPO_TESTNET_CHAIN_ID, 42_431);
    }

    #[test]
    fn default_receipt_timeout_mainnet_is_three_minutes() {
        assert_eq!(default_receipt_timeout(4_217), Duration::from_mins(3));
    }

    #[test]
    fn default_receipt_timeout_testnet_is_one_minute() {
        assert_eq!(default_receipt_timeout(42_431), Duration::from_mins(1));
    }

    #[test]
    fn default_receipt_timeout_unknown_chain_falls_back_to_testnet_budget() {
        // Dev / anvil chains get the shorter budget — mirrors `default_rpc_for`.
        assert_eq!(default_receipt_timeout(31_337), Duration::from_mins(1));
    }

    #[test]
    fn deadline_below_minimum_detects_only_that_code() {
        assert!(is_deadline_below_minimum(
            &taskfast_client::Error::Validation {
                code: "deadline_below_minimum".into(),
                message: "x".into(),
            }
        ));
        assert!(!is_deadline_below_minimum(
            &taskfast_client::Error::Validation {
                code: "validation_error".into(),
                message: "x".into(),
            }
        ));
        assert!(!is_deadline_below_minimum(&taskfast_client::Error::Server(
            "deadline_below_minimum".into()
        )));
    }
}
