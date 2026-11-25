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
//!  5. EIP-712 `DistributionApproval(escrowId, deadline)` — `sign_distribution`;
//!     skipped when the params carry `review_window_seconds` (v3 venue).
//!  6. `TaskEscrow.open` / `openWithMemo` — or `openWithReview` when the params
//!     carry `review_window_seconds` (v3 venue) — broadcast + wait for receipt.
//!  7. `POST /bids/:id/escrow/finalize` with voucher (+ sig + deadline without a review window).
//!
//! The params request always sends `review_clock=2`: a v3 venue refuses it
//! otherwise (409 `review_clock_client_required`).
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
use alloy_sol_types::SolCall;
use clap::Parser;
use serde_json::json;
use uuid::Uuid;

use super::{
    is_proxy_rpc_url, network_policy_for_chain_id, parse_duration, validate_override_rpc_url,
    CmdError, CmdResult, Ctx,
};
use crate::envelope::Envelope;

use taskfast_agent::chain::{
    compute_escrow_id, decode_escrow_state, decode_escrow_worker, decode_u256, get_escrow_calldata,
    EscrowIdParams, TaskEscrow,
};
use taskfast_agent::tempo_rpc::TempoRpcClient;
use taskfast_chains::tempo::{sign_distribution, DistributionDomain};
use taskfast_client::api::types::{
    BidEscrowFinalizeRequest, BidEscrowFinalizeRequestPosterApprovalDeadline,
    BidEscrowParamsResponse, GetBidEscrowParamsReviewClock,
};
use taskfast_client::{map_api_error, TaskFastClient};

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Sign + broadcast the on-chain escrow for a deferred bid; POST finalize.
    Sign(SignArgs),
    /// Worker: send `submitWork(escrowId)` from your wallet so the review clock
    /// starts on chain. `task submit` / `task remedy` already do this; run it
    /// to retry after one reports `handed_in_clock_not_started`. Safe to repeat.
    SubmitWork(SubmitWorkArgs),
}

/// Wallet/RPC flags as for `escalate timeout-resolve`; the task must be one you work on.
#[derive(Debug, Parser)]
pub struct SubmitWorkArgs {
    /// Task UUID. Must be `under_review`; caller must be the escrow's worker.
    pub task_id: String,

    /// Keystore reference (same form as `taskfast escrow sign`).
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

    /// Receipt-polling ceiling — human duration (`3min`, `90s`).
    #[arg(
        long,
        env = "TASKFAST_RECEIPT_TIMEOUT",
        value_parser = super::parse_duration,
    )]
    pub receipt_timeout: Option<std::time::Duration>,
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
        value_parser = parse_duration,
    )]
    pub receipt_timeout: Option<Duration>,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Sign(a) => sign(ctx, a).await,
        Command::SubmitWork(a) => {
            let opts = WalletOpts {
                keystore: a.keystore.as_deref(),
                password_file: a.wallet_password_file.as_deref(),
                wallet_address: a.wallet_address.as_deref(),
                rpc_url: a.rpc_url.as_deref(),
                receipt_timeout: a.receipt_timeout,
            };
            let task_id = Uuid::parse_str(&a.task_id)
                .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
            let data = submit_work(ctx, &task_id, &opts, false).await?;
            Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
        }
    }
}

async fn sign(ctx: &Ctx, args: SignArgs) -> CmdResult {
    // 1. Validate bid UUID locally.
    let bid_id = Uuid::parse_str(&args.bid_id)
        .map_err(|e| CmdError::Usage(format!("bid id must be a UUID: {e}")))?;

    let client = ctx.client()?;

    // 2. Fetch escrow params — server enforces poster-auth + bid status.
    let params = match fetch_params(&client, &bid_id).await {
        Ok(v) => v,
        Err(mapped) => {
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
            // Any other undecodable body (e.g. a non-integer or oversized
            // `review_window_seconds`): say plainly that nothing was sent.
            if let taskfast_client::Error::Decode(m) = &mapped {
                return Err(CmdError::Decode(format!(
                    "server escrow params response is malformed ({m}); no transaction was sent"
                )));
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

    // 5b. Review window (v3 venue). Validated here, before the keystore, any
    //     approve tx or the dry-run envelope: a bad window must send nothing.
    let review_window = review_window_from(&params)?;
    let open_deadline = open_approval_deadline(&params)?;

    // 6. Load signer + preflight address equality.
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
    let mut approval = open_deadline
        .map(|d| sign_distribution(&signer, &domain, escrow_id, U256::from(d)).map(|sig| (d, sig)))
        .transpose()?;

    // 9. Build open / openWithMemo calldata up front — we reuse it for the
    //    dry-run envelope and the live broadcast.
    let open_calldata = open_calldata(
        &OpenFields {
            token: token_address,
            deposit,
            worker,
            platform_fee,
            platform: platform_wallet,
            arbitrator,
            salt,
        },
        memo_hash_opt,
        review_window,
    );

    // Resolve RPC URL. Override flows to a bare upstream gateway; default
    // path pulls the deployment's proxy URL from `GET /config/network`
    // (reverse-lookup by chain_id, so the selected entry matches the
    // server-returned escrow params). The proxy URL is sanity-checked to
    // live under the authenticated api_base — catches a misconfigured (or
    // malicious) deployment returning an off-host upstream.
    let (rpc_url, _via_proxy) = if let Some(ref url) = args.rpc_url {
        validate_override_rpc_url(
            url,
            network_policy_for_chain_id(chain_id_u64),
            ctx.allow_custom_endpoints,
        )?;
        (url.clone(), false)
    } else {
        let cfg = client.fetch_network_config().await.map_err(|e| {
            CmdError::Server(format!("fetch network config from {}: {e}", ctx.base_url()))
        })?;
        let (_name, entry) = cfg.entry_by_chain_id(params.chain_id).map_err(|e| {
            CmdError::Server(format!(
                "deployment at {} does not advertise chain_id={}: {e}",
                ctx.base_url(),
                params.chain_id
            ))
        })?;
        // Same-host guard as post::resolve_proxy_rpc_url — the proxy may be mounted
        // at any path on api_base (`/rpc/…` or `/api/rpc/…`); only the host
        // must match so the API key never leaves the approved deployment.
        if !is_proxy_rpc_url(&entry.rpc_url, ctx.base_url()) {
            return Err(CmdError::Server(format!(
                "deployment at {} returned rpc_url {:?} for chain_id={}, \
                 which is not on the same host. Refusing to route RPC \
                 traffic off-host.",
                ctx.base_url(),
                entry.rpc_url,
                params.chain_id,
            )));
        }
        (entry.rpc_url.clone(), true)
    };

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
                "deadline": approval.as_ref().map(|(d, _)| *d),
                "signature": approval.as_ref().map(|(_, s)| s),
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

    let http = ctx.rpc_http_client(&client, &rpc_url);
    let rpc = TempoRpcClient::new(http, rpc_url);
    let default_timeout = crate::tempo_session::default_receipt_timeout_for_chain_id(chain_id_u64);
    let receipt_timeout =
        super::resolve_duration(args.receipt_timeout, ctx.receipt_timeout, default_timeout);
    let session =
        crate::tempo_session::TempoSession::new(signer, keystore_path, rpc, receipt_timeout);

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
        let (poster_approval_signature, poster_approval_deadline) = match &approval {
            Some((d, sig)) => {
                let deadline_i64 = i64::try_from(*d)
                    .map_err(|_| CmdError::Decode(format!("deadline {d} exceeds i64 range")))?;
                (
                    Some(sig.clone()),
                    Some(BidEscrowFinalizeRequestPosterApprovalDeadline::Integer(
                        deadline_i64,
                    )),
                )
            }
            None => (None, None),
        };
        let body = BidEscrowFinalizeRequest {
            voucher: voucher_hex.clone(),
            poster_approval_signature,
            poster_approval_deadline,
            memo_hash: params.memo_hash.clone(),
        };
        match client.inner().finalize_bid_escrow(&bid_id, &body).await {
            Ok(r) => break r.into_inner(),
            Err(e) => {
                let mapped = map_api_error(e).await;
                if attempts == 0 && approval.is_some() && is_deadline_below_minimum(&mapped) {
                    attempts += 1;
                    let fresh = fetch_params(&client, &bid_id).await?;
                    let deadline = server_deadline_or_abort(&fresh)?;
                    let sig = sign_distribution(
                        session.signer(),
                        &domain,
                        escrow_id,
                        U256::from(deadline),
                    )?;
                    approval = Some((deadline, sig));
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
            "deadline": approval.as_ref().map(|(d, _)| *d),
        }),
    ))
}

/// `GET /bids/:id/escrow/params`, always declaring the review clock. Every
/// fetch goes through here so none can forget the flag (a v3 venue answers 409
/// `review_clock_client_required` without it).
async fn fetch_params(
    client: &TaskFastClient,
    bid_id: &Uuid,
) -> Result<BidEscrowParamsResponse, taskfast_client::Error> {
    match client
        .inner()
        .get_bid_escrow_params(bid_id, Some(GetBidEscrowParamsReviewClock::X2))
        .await
    {
        Ok(v) => Ok(v.into_inner()),
        Err(e) => Err(map_api_error(e).await),
    }
}

/// Largest window the browser accepts (`Number.isSafeInteger`). A client-side
/// sanity bound only; the 1 h–72 h policy bound is the server's and the contract's.
const MAX_REVIEW_WINDOW_SECONDS: i64 = (1 << 53) - 1;

/// `None` (absent or null) → legacy `open`. A present window must be a
/// positive safe integer or we abort before any transaction (browser parity).
fn review_window_from(params: &BidEscrowParamsResponse) -> Result<Option<U256>, CmdError> {
    match params.review_window_seconds {
        None => Ok(None),
        Some(s) if (1..=MAX_REVIEW_WINDOW_SECONDS).contains(&s) => Ok(Some(U256::from(s))),
        Some(s) => Err(CmdError::Validation {
            code: "invalid_review_window".into(),
            message: format!(
                "server review_window_seconds={s} must be a positive integer \
                 (at most {MAX_REVIEW_WINDOW_SECONDS}); refusing to open the escrow"
            ),
        }),
    }
}

// Private so no `TaskEscrowCalls` variant appears in the public `TaskEscrow`
// block (cargo-semver-checks `enum_variant_added`).
mod review_abi {
    alloy_sol_types::sol! {
        function submitWork(bytes32 escrowId) external;
        function reviewWindowOf(bytes32 escrowId) external view returns (uint256);
        function submittedAtOf(bytes32 escrowId) external view returns (uint256);
        function openWithReview(
            address token,
            uint256 deposit,
            address worker,
            uint256 platformFeeAmount,
            address platform,
            address arbitrator,
            bytes32 salt,
            uint256 reviewWindow,
            bytes32 memoHash
        ) external returns (bytes32);
    }
}

struct OpenFields {
    token: Address,
    deposit: U256,
    worker: Address,
    platform_fee: U256,
    platform: Address,
    arbitrator: Address,
    salt: B256,
}

/// Open calldata: `openWithReview` when a window is issued (memo word is
/// `bytes32(0)` when there is no memo, as in the browser), else `openWithMemo`
/// / `open` exactly as before.
fn open_calldata(f: &OpenFields, memo: Option<B256>, review_window: Option<U256>) -> Bytes {
    if let Some(window) = review_window {
        return review_abi::openWithReviewCall {
            token: f.token,
            deposit: f.deposit,
            worker: f.worker,
            platformFeeAmount: f.platform_fee,
            platform: f.platform,
            arbitrator: f.arbitrator,
            salt: f.salt,
            reviewWindow: window,
            memoHash: memo.unwrap_or_default(),
        }
        .abi_encode()
        .into();
    }
    if let Some(memo_hash) = memo {
        TaskEscrow::openWithMemoCall {
            token: f.token,
            deposit: f.deposit,
            worker: f.worker,
            platformFeeAmount: f.platform_fee,
            platform: f.platform,
            arbitrator: f.arbitrator,
            salt: f.salt,
            memoHash: memo_hash,
        }
        .abi_encode()
        .into()
    } else {
        TaskEscrow::openCall {
            token: f.token,
            deposit: f.deposit,
            worker: f.worker,
            platformFeeAmount: f.platform_fee,
            platform: f.platform,
            arbitrator: f.arbitrator,
            salt: f.salt,
        }
        .abi_encode()
        .into()
    }
}

/// Return the server-issued `poster_approval_deadline` (Unix seconds) after the
/// defense-in-depth floor check: if it already carries less than
/// `poster_approval_min_lifetime` remaining, abort so no tx is ever broadcast
/// against an approval the server would reject at finalize (gh#118). This should
/// never fire when params are fetched immediately before signing — it catches a
/// stale fetch, clock skew, or a retry.
fn server_deadline_or_abort(params: &BidEscrowParamsResponse) -> Result<u64, CmdError> {
    let (Some(raw_deadline), Some(raw_min_lifetime)) = (
        params.poster_approval_deadline,
        params.poster_approval_min_lifetime,
    ) else {
        return Err(CmdError::Validation {
            code: "server_missing_approval_deadline".into(),
            message: "server omitted `poster_approval_deadline`; this deployment predates \
                      the server-issued escrow deadline — upgrade the server or use a CLI \
                      version that matches it"
                .into(),
        });
    };
    let deadline = u64::try_from(raw_deadline).map_err(|_| {
        CmdError::Decode(format!(
            "server poster_approval_deadline={raw_deadline} is negative"
        ))
    })?;
    let min_lifetime = u64::try_from(raw_min_lifetime).map_err(|_| {
        CmdError::Decode(format!(
            "server poster_approval_min_lifetime={raw_min_lifetime} is negative"
        ))
    })?;
    let now = u64::try_from(jiff::Timestamp::now().as_second())
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
    Ok(deadline)
}

fn open_approval_deadline(params: &BidEscrowParamsResponse) -> Result<Option<u64>, CmdError> {
    if params.review_window_seconds.is_some() {
        return Ok(None);
    }
    server_deadline_or_abort(params).map(Some)
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

// ─── worker `submitWork` (taskfast#2044) ──────────────────────────────────

/// Wallet/RPC flags, same sources as `escrow sign` and `settle` (flag, env,
/// then `Ctx` config). Used by `escrow submit-work` and by `task submit`/`remedy`.
pub(super) struct WalletOpts<'a> {
    pub keystore: Option<&'a str>,
    pub password_file: Option<&'a std::path::Path>,
    pub wallet_address: Option<&'a str>,
    pub rpc_url: Option<&'a str>,
    pub receipt_timeout: Option<std::time::Duration>,
}

/// `EscrowState::Open` in `ArbitratedEscrow.sol`.
const ESCROW_STATE_OPEN: u8 = 1;

pub(super) fn submit_work_calldata(escrow_id: B256) -> Bytes {
    review_abi::submitWorkCall {
        escrowId: escrow_id,
    }
    .abi_encode()
    .into()
}

/// True when the chain already holds this hand-in. The server sets
/// `review_deadline = hand-in time + window`, and the chain send follows the
/// hand-in, so a send for the current hand-in always lands at or after it;
/// an earlier hand-in's `submittedAt + window` is strictly below.
fn already_recorded(submitted_at: U256, window: U256, review_deadline: U256) -> bool {
    !submitted_at.is_zero() && submitted_at.saturating_add(window) >= review_deadline
}

fn view_calldata(escrow_id: B256, window: bool) -> Bytes {
    if window {
        review_abi::reviewWindowOfCall {
            escrowId: escrow_id,
        }
        .abi_encode()
        .into()
    } else {
        review_abi::submittedAtOfCall {
            escrowId: escrow_id,
        }
        .abi_encode()
        .into()
    }
}

/// Send `submitWork` for the task's current hand-in. Returns the envelope data.
///
/// Order: window read (legacy escrow, window 0, returns before any wallet is
/// loaded) → task status → signer → escrow state + worker → duplicate check →
/// send. `auto` is the call after a server-accepted hand-in: a task with no
/// escrow yet, or already `complete` (auto-approve), has no clock to start.
pub(super) async fn submit_work(
    ctx: &Ctx,
    task_id: &Uuid,
    opts: &WalletOpts<'_>,
    auto: bool,
) -> Result<serde_json::Value, CmdError> {
    let client = ctx.client()?;
    let task = match client.inner().get_task(task_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    let skipped = |reason: &str| json!({ "action": "skipped", "reason": reason });
    if auto && task.escrow_id.is_none() {
        return Ok(skipped("task has no escrow to start a review clock on"));
    }
    let target = super::escalate::chain_target(&task, "nothing to submit")?;
    let (contract, escrow_id) = (target.contract, target.escrow_id);
    let (rpc, _) = super::escalate::connect_rpc(ctx, &client, &target, opts.rpc_url).await?;
    let read = |what: &'static str, data: Bytes| {
        let rpc = &rpc;
        async move {
            rpc.eth_call(contract, &data)
                .await
                .map_err(|e| super::escalate::chain_read_err(what, e))
        }
    };

    // A contract from before the review clock has no `reviewWindowOf`: the call
    // reverts (JSON-RPC execution error) or returns nothing. That is a legacy
    // escrow, not a failure; transport and decode errors still are.
    let window = match rpc
        .eth_call(contract, &view_calldata(escrow_id, true))
        .await
    {
        Ok(b) if b.is_empty() => U256::ZERO,
        Ok(b) => {
            decode_u256(&b).map_err(|e| super::escalate::chain_read_err("reviewWindowOf", e))?
        }
        Err(taskfast_agent::tempo_rpc::RpcError::Rpc { .. }) => U256::ZERO,
        Err(e) => return Err(super::escalate::chain_read_err("reviewWindowOf", e)),
    };
    if window.is_zero() {
        return Ok(json!({
            "action": "no_review_clock",
            "task_id": task_id.to_string(),
            "note": "this escrow has no review clock (legacy escrow); nothing to send",
        }));
    }
    let status = task.status.as_deref().unwrap_or("");
    if status != "under_review" {
        if auto && status == "complete" {
            return Ok(skipped("task is already complete; no review clock needed"));
        }
        return Err(CmdError::Validation {
            code: "task_not_under_review".into(),
            message: format!(
                "task status is `{status}`; the review clock starts only for a handed-in \
                 (`under_review`) task. Nothing sent"
            ),
        });
    }
    let review_deadline = task
        .review_deadline
        .and_then(|t| u64::try_from(t.as_second()).ok())
        .map(U256::from)
        .ok_or_else(|| CmdError::Validation {
            code: "review_deadline_missing".into(),
            message: "under_review task has no review_deadline; cannot tell whether this \
                      hand-in is already on chain. Nothing sent"
                .into(),
        })?;

    let keystore_path =
        super::wallet_args::resolve_keystore_path(opts.keystore, ctx.keystore_path.as_deref());
    let signer = super::wallet_args::load_signer_and_validate(
        keystore_path.as_deref(),
        opts.password_file,
        opts.wallet_address.or(ctx.wallet_address.as_deref()),
        "escrow submit-work",
    )?;
    let signer_address = signer.address();

    let state_raw = read("getEscrow", get_escrow_calldata(escrow_id).into()).await?;
    let state = decode_escrow_state(&state_raw)
        .map_err(|e| super::escalate::chain_read_err("getEscrow", e))?;
    if state != ESCROW_STATE_OPEN {
        return Err(CmdError::Validation {
            code: "escrow_not_open".into(),
            message: format!(
                "escrow state on chain is {state} (Open=1, PendingRefund=2, Completed=3, \
                 Refunded=4, Disputed=5); submitWork needs an Open escrow. Nothing sent"
            ),
        });
    }
    let worker = decode_escrow_worker(&state_raw)
        .map_err(|e| super::escalate::chain_read_err("getEscrow", e))?;
    if worker != signer_address {
        return Err(CmdError::Validation {
            code: "not_escrow_worker".into(),
            message: format!(
                "wallet {signer_address:#x} is not the escrow's worker {worker:#x}; only the \
                 worker can submit work. Nothing sent"
            ),
        });
    }
    let submitted_at = decode_u256(&read("submittedAtOf", view_calldata(escrow_id, false)).await?)
        .map_err(|e| super::escalate::chain_read_err("submittedAtOf", e))?;
    let common = |action: &str| {
        json!({
            "action": action,
            "task_id": task_id.to_string(),
            "escrow_id": format!("{escrow_id:#x}"),
            "to": format!("{contract:#x}"),
            "chain_id": target.chain_id,
            "from": format!("{signer_address:#x}"),
            "window": window.to_string(),
            "submitted_at": submitted_at.to_string(),
        })
    };
    if already_recorded(submitted_at, window, review_deadline) {
        return Ok(common("already_submitted"));
    }
    let calldata = submit_work_calldata(escrow_id);
    let mut data = common(if ctx.dry_run {
        "would_submit_work"
    } else {
        "submit_work_submitted"
    });
    data["calldata"] = json!(format!("0x{}", hex::encode(&calldata)));
    if ctx.dry_run {
        return Ok(data);
    }

    let receipt_timeout = super::resolve_duration(
        opts.receipt_timeout,
        ctx.receipt_timeout,
        crate::tempo_session::default_receipt_timeout_for_chain_id(target.chain_id_u64),
    );
    let session =
        crate::tempo_session::TempoSession::new(signer, keystore_path, rpc, receipt_timeout);
    let tx = session
        .send_transaction_and_wait(
            contract,
            calldata,
            "submit-work",
            Some(
                " (InvalidState: escrow not Open; Unauthorized: not the worker wallet; \
                 InvalidReviewWindow: no review clock; SubmissionLimitReached: 4 hand-ins already recorded)",
            ),
        )
        .await?;
    data["tx_hash"] = json!(format!("{tx:#x}"));
    data["note"] =
        json!("the review clock counts from the mined block; TaskFast reads it from chain");
    Ok(data)
}

/// Chain step after a server-accepted hand-in (`task submit` / `task remedy`).
/// Any failure here is NOT a failed hand-in: it becomes
/// [`CmdError::HandedInClockNotStarted`] naming the retry command.
pub(super) async fn submit_work_after_handin(
    ctx: &Ctx,
    task_id: &Uuid,
    opts: &WalletOpts<'_>,
) -> Result<serde_json::Value, CmdError> {
    submit_work(ctx, task_id, opts, true).await.map_err(|e| {
        CmdError::HandedInClockNotStarted(format!(
            "your work IS handed in (the server accepted it), but the on-chain review \
                 clock did NOT start: {e}. Do not submit again. Retry only the chain step: \
                 `taskfast escrow submit-work {task_id}` (safe to run more than once)"
        ))
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn submit_work_selector_and_calldata_are_pinned() {
        // Golden: `cast sig "submitWork(bytes32)"` = 0x3a9cfef2.
        let id = B256::repeat_byte(0xab);
        let cd = submit_work_calldata(id);
        assert_eq!(hex::encode(&cd[..4]), "3a9cfef2");
        assert_eq!(cd.len(), 36);
        assert_eq!(&cd[4..], id.as_slice());
        let sel = |b: &Bytes| hex::encode(&b[..4]);
        assert_eq!(sel(&view_calldata(id, true)), "ec853c7d"); // reviewWindowOf
        assert_eq!(sel(&view_calldata(id, false)), "53fff8ac"); // submittedAtOf
    }

    #[test]
    fn already_recorded_boundaries() {
        let (w, d) = (U256::from(100u64), U256::from(1000u64));
        assert!(already_recorded(U256::from(900u64), w, d)); // equal: recorded
        assert!(!already_recorded(U256::from(899u64), w, d)); // earlier hand-in
        assert!(!already_recorded(U256::ZERO, w, U256::ZERO)); // never submitted, even at deadline 0
    }
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

    fn fields() -> OpenFields {
        OpenFields {
            token: Address::repeat_byte(1),
            deposit: U256::from(75_000_000u64),
            worker: Address::repeat_byte(3),
            platform_fee: U256::from(3_750_000u64),
            platform: Address::repeat_byte(4),
            arbitrator: Address::repeat_byte(5),
            salt: B256::repeat_byte(9),
        }
    }

    #[test]
    fn open_with_review_selector_is_pinned() {
        use alloy_primitives::keccak256;
        let sig = "openWithReview(address,uint256,address,uint256,address,address,bytes32,uint256,bytes32)";
        assert_eq!(
            review_abi::openWithReviewCall::SELECTOR,
            keccak256(sig)[..4]
        );
        assert_eq!(
            review_abi::openWithReviewCall::SELECTOR,
            [0xf1, 0xe3, 0x2b, 0xb8]
        );
    }

    #[test]
    fn legacy_calldata_is_byte_identical_without_window() {
        let f = fields();
        let memo = B256::repeat_byte(0x11);
        let open = TaskEscrow::openCall {
            token: f.token,
            deposit: f.deposit,
            worker: f.worker,
            platformFeeAmount: f.platform_fee,
            platform: f.platform,
            arbitrator: f.arbitrator,
            salt: f.salt,
        }
        .abi_encode();
        let with_memo = TaskEscrow::openWithMemoCall {
            token: f.token,
            deposit: f.deposit,
            worker: f.worker,
            platformFeeAmount: f.platform_fee,
            platform: f.platform,
            arbitrator: f.arbitrator,
            salt: f.salt,
            memoHash: memo,
        }
        .abi_encode();
        assert_eq!(open_calldata(&f, None, None).to_vec(), open);
        assert_eq!(open_calldata(&f, Some(memo), None).to_vec(), with_memo);
    }

    #[test]
    fn review_calldata_layout_matches_browser_encoding() {
        let f = fields();
        let memo = B256::repeat_byte(0x11);
        let cd = open_calldata(&f, Some(memo), Some(U256::from(86_400u64)));
        assert_eq!(cd[..4], [0xf1, 0xe3, 0x2b, 0xb8]);
        assert_eq!(cd.len(), 4 + 32 * 9);
        assert_eq!(&cd[4 + 32 * 6..4 + 32 * 7], f.salt.as_slice());
        assert_eq!(
            U256::from_be_slice(&cd[4 + 32 * 7..4 + 32 * 8]),
            U256::from(86_400u64)
        );
        assert_eq!(&cd[4 + 32 * 8..], memo.as_slice());
        let no_memo = open_calldata(&f, None, Some(U256::from(1u64)));
        assert_eq!(&no_memo[4 + 32 * 8..], B256::ZERO.as_slice());
    }
}
