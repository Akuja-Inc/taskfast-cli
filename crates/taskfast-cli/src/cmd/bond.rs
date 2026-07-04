// SPDX-License-Identifier: MIT
//! `taskfast bond post <task_id>` — auction-task operator posts the on-chain
//! performance bond in one step (gh#95).
//!
//! `taskfast stake` is HA-direct-only (auction tasks reject with
//! `not_high_assurance`); the working bond flow for an auction task had to be
//! hand-run with raw `cast`. This command drives it end to end:
//!
//!  1. `GET /tasks/:id/stake/quote` → `required_amount` (bond base units).
//!  2. Resolve the bond ERC-20 token + chain from `GET /config/network`
//!     (`default_stablecoin`), and the TaskBond contract from `--task-bond`.
//!  3. Derive `taskRef` (16 zero bytes ++ task UUID) + a random `salt`.
//!  4. ERC-20 `allowance` preflight; `approve` the TaskBond contract if short.
//!  5. `TaskBond.post(token, amount, taskRef, salt)` — broadcast + wait receipt.
//!  6. `POST /tasks/:id/stake/report {tx_hash}` — a claim, not proof.
//!  7. Poll the quote until `bond_status=posted` (server verifies `BondPosted`
//!     asynchronously).
//!
//! The server exposes neither the bond token address (beyond the deployment
//! `default_stablecoin`) nor the TaskBond contract address, and the quote does
//! not yet return `task_ref`/`salt` — so token defaults to `default_stablecoin`
//! (override with `--token`) and the TaskBond address is a required flag. The
//! verifier matches contract/token/taskRef/amount and does not pin salt, so a
//! random salt is accepted.
//!
//! Error mapping delegates to `map_api_error` (401|403→Auth, 409|422→
//! Validation). Keystore / signing failures surface as `Wallet` (exit 5).

use std::path::PathBuf;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use clap::{Parser, ValueEnum};
use serde_json::json;
use uuid::Uuid;

use super::{
    is_proxy_rpc_url, resolve_duration, validate_override_rpc_url, CmdError, CmdResult, Ctx,
};
use crate::envelope::Envelope;

use taskfast_agent::chain::{compute_task_ref, TaskBond, IERC20};
use taskfast_agent::tempo_rpc::{sign_and_broadcast_tx, TempoRpcClient};
use taskfast_client::api::types::{
    GetStakeQuoteStakeSource, ReportStakePostTxBody, ReportStakePostTxBodyStakeSource,
};
use taskfast_client::map_api_error;

/// Tempo mainnet chain ID — mirrors the `escrow sign` receipt-timeout default
/// (mainnet gets a longer ceiling; testnet/unknown share the shorter one).
const TEMPO_MAINNET_CHAIN_ID: i64 = 4217;

/// `TaskBond.post` receipt polling defaults (same shape as `escrow sign`):
/// 3min mainnet, 1min testnet, 1min for unknown chains.
const DEFAULT_RECEIPT_TIMEOUT_MAINNET: Duration = Duration::from_mins(3);
const DEFAULT_RECEIPT_TIMEOUT_TESTNET: Duration = Duration::from_mins(1);
const RECEIPT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Server-side `BondPosted` verification is asynchronous, so after reporting we
/// re-quote until `bond_status=posted`. Bounded so the command still returns if
/// verification lags; a timeout is reported as `posting`, not a failure.
const DEFAULT_VERIFY_TIMEOUT: Duration = Duration::from_mins(1);
const VERIFY_POLL_INTERVAL: Duration = Duration::from_secs(3);

fn default_receipt_timeout(chain_id: i64) -> Duration {
    match chain_id {
        TEMPO_MAINNET_CHAIN_ID => DEFAULT_RECEIPT_TIMEOUT_MAINNET,
        _ => DEFAULT_RECEIPT_TIMEOUT_TESTNET,
    }
}

/// clap-friendly mirror of the generated stake-source enums (kept local, like
/// `stake::StakeSource`, so codegen needn't grow clap derives). kebab-case
/// renders `operator-self` / `external-backer`.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum BondStakeSource {
    OperatorSelf,
    ExternalBacker,
}

impl BondStakeSource {
    fn to_quote(self) -> GetStakeQuoteStakeSource {
        match self {
            Self::OperatorSelf => GetStakeQuoteStakeSource::OperatorSelf,
            Self::ExternalBacker => GetStakeQuoteStakeSource::ExternalBacker,
        }
    }

    fn to_report(self) -> ReportStakePostTxBodyStakeSource {
        match self {
            Self::OperatorSelf => ReportStakePostTxBodyStakeSource::OperatorSelf,
            Self::ExternalBacker => ReportStakePostTxBodyStakeSource::ExternalBacker,
        }
    }
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Quote, approve, post `TaskBond.post`, and report — for an auction task.
    Post(PostArgs),
}

#[derive(Debug, Parser)]
pub struct PostArgs {
    /// Target task UUID. The auction task whose operator bond you are posting.
    pub task_id: String,

    /// TaskBond contract address (`0x`-prefixed). The server does not advertise
    /// it. On Tempo Moderato testnet this is
    /// `0x31de2fd7d1d4bfcfb3d2b4bfc30f6b46f2b55db2`.
    #[arg(long, env = "TASKFAST_TASK_BOND_ADDRESS")]
    pub task_bond: String,

    /// Bond ERC-20 token address (`0x`-prefixed). Defaults to the deployment's
    /// `default_stablecoin` from `GET /config/network`.
    #[arg(long)]
    pub token: Option<String>,

    /// Override the posted amount (bond base units). Defaults to the quote's
    /// `required_amount`. Must be at least the required amount — less never
    /// verifies.
    #[arg(long)]
    pub amount: Option<i64>,

    /// Who is posting. `operator-self` (default) authenticates with your agent
    /// key; `external-backer` authenticates as the bond's recorded backer.
    #[arg(long, value_enum, default_value_t = BondStakeSource::OperatorSelf)]
    pub source: BondStakeSource,

    /// Keystore reference (same form as `taskfast escrow sign`).
    #[arg(long, env = "TEMPO_KEY_SOURCE")]
    pub keystore: Option<String>,

    /// Path to keystore password file.
    #[arg(long, env = "TASKFAST_WALLET_PASSWORD_FILE")]
    pub wallet_password_file: Option<PathBuf>,

    /// Wallet address preflight (`0x`-prefixed). When set, fail before touching
    /// the chain if the keystore decrypts to a mismatch.
    #[arg(long, env = "TEMPO_WALLET_ADDRESS")]
    pub wallet_address: Option<String>,

    /// Tempo RPC override. Defaults to the deployment's authenticated proxy URL.
    #[arg(long, env = "TEMPO_RPC_URL")]
    pub rpc_url: Option<String>,

    /// Skip the on-chain `allowance` preflight + `approve` tx. Only safe when a
    /// sufficient allowance was already granted to the TaskBond contract.
    #[arg(long)]
    pub skip_allowance_check: bool,

    /// Receipt-polling ceiling — human duration (`3min`, `90s`). Falls back to
    /// `receipt_timeout` in config, then a chain-aware default.
    #[arg(long, env = "TASKFAST_RECEIPT_TIMEOUT", value_parser = humantime::parse_duration)]
    pub receipt_timeout: Option<Duration>,

    /// How long to poll the quote for `bond_status=posted` after reporting.
    /// A timeout returns `posting` (the tx is on-chain; verification is async),
    /// not an error.
    #[arg(long, env = "TASKFAST_BOND_VERIFY_TIMEOUT", value_parser = humantime::parse_duration)]
    pub verify_timeout: Option<Duration>,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Post(a) => post(ctx, a).await,
    }
}

async fn post(ctx: &Ctx, args: PostArgs) -> CmdResult {
    // 1. Validate the task UUID before any HTTP so typos never cost a round-trip.
    let task_id = Uuid::parse_str(&args.task_id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;

    let client = ctx.client()?;

    // 2. Quote. Server prices from task value; if a bond already exists the
    //    recorded pricing is authoritative. An already-`posted` bond is a no-op.
    let quote = match client
        .inner()
        .get_stake_quote(&task_id, Some(args.source.to_quote()))
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    if quote.bond_status.as_deref() == Some("posted") {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "already_posted",
                "task_id": task_id.to_string(),
                "bond_status": "posted",
            }),
        ));
    }

    // 3. Resolve amount (base units). Default to the required amount; a smaller
    //    override never verifies, so reject it before spending gas.
    let amount_i64 = args.amount.unwrap_or(quote.required_amount);
    if amount_i64 < quote.required_amount {
        return Err(CmdError::Usage(format!(
            "--amount {amount_i64} is below the quoted required_amount {}; a smaller bond never verifies",
            quote.required_amount
        )));
    }
    let amount = U256::from(u64::try_from(amount_i64).map_err(|_| {
        CmdError::Usage(format!(
            "amount {amount_i64} must be a non-negative integer"
        ))
    })?);
    if amount.is_zero() {
        return Err(CmdError::Usage("bond amount resolves to 0".into()));
    }

    // 4. Parse the TaskBond contract (required; not server-advertised).
    let task_bond: Address = args.task_bond.parse().map_err(|e| {
        CmdError::Usage(format!(
            "--task-bond `{}` is not a valid EVM address: {e}",
            args.task_bond
        ))
    })?;

    // 5. Load signer + preflight the wallet address.
    let keystore_ref = args.keystore.as_deref().map(str::to_string).or_else(|| {
        ctx.keystore_path
            .as_deref()
            .and_then(|p| p.to_str().map(str::to_string))
    });
    let signer = super::wallet_args::load_signer(
        keystore_ref.as_deref(),
        args.wallet_password_file.as_deref(),
        "bond post",
    )?;
    let wallet_address_for_check = args
        .wallet_address
        .as_deref()
        .or(ctx.wallet_address.as_deref());
    if let Some(expected) = wallet_address_for_check {
        let expected_addr: Address = expected.parse().map_err(|e| {
            CmdError::Usage(format!("--wallet-address is not a valid EVM address: {e}"))
        })?;
        if signer.address() != expected_addr {
            return Err(CmdError::Usage(format!(
                "keystore address {:#x} does not match --wallet-address {expected}",
                signer.address(),
            )));
        }
    }

    // 6. Resolve chain + token + RPC URL from the deployment's network config.
    let (rpc_url, chain_id, default_stablecoin) = resolve_network(ctx, &client, &args).await?;
    let token: Address = match args.token.as_deref() {
        Some(t) => t.parse().map_err(|e| {
            CmdError::Usage(format!("--token `{t}` is not a valid EVM address: {e}"))
        })?,
        None => {
            let ds = default_stablecoin.ok_or_else(|| {
                CmdError::Usage(
                    "no --token given and the deployment advertises no default_stablecoin; \
                     pass --token <erc20-address>"
                        .into(),
                )
            })?;
            ds.parse().map_err(|e| {
                CmdError::Server(format!(
                    "deployment default_stablecoin `{ds}` is not a valid EVM address: {e}"
                ))
            })?
        }
    };

    // 7. Derive taskRef (server verifier layout) + a random salt.
    let task_ref = compute_task_ref(task_id);
    let salt = B256::from(rand::random::<[u8; 32]>());

    // 8. Build TaskBond.post calldata up front — reused for dry-run + broadcast.
    let post_calldata: Bytes = TaskBond::postCall {
        token,
        amount,
        taskRef: task_ref,
        salt,
    }
    .abi_encode()
    .into();

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_post_bond",
                "task_id": task_id.to_string(),
                "task_bond": format!("{task_bond:#x}"),
                "token": format!("{token:#x}"),
                "amount": amount_i64,
                "task_ref": format!("{task_ref:#x}"),
                "salt": format!("{salt:#x}"),
                "post_calldata": format!("0x{}", hex::encode(&post_calldata)),
                "rpc_url": rpc_url,
                "chain_id": chain_id,
            }),
        ));
    }

    // Receipt-timeout precedence: flag > config > chain-aware default.
    let receipt_timeout = resolve_duration(
        args.receipt_timeout,
        ctx.receipt_timeout,
        default_receipt_timeout(chain_id),
    );

    // 9. Live RPC: balance + allowance preflight, approve TaskBond if short.
    let http = ctx.rpc_http_client(&client, &rpc_url);
    let rpc = TempoRpcClient::new(http, rpc_url.clone());
    let mut approval_tx_hex: Option<String> = None;
    if !args.skip_allowance_check {
        let balance = erc20_balance_of(&rpc, token, signer.address()).await?;
        if balance < amount {
            return Err(CmdError::Usage(format!(
                "poster balance {balance} < required bond {amount} on token {token:#x}"
            )));
        }
        let current_allowance = erc20_allowance(&rpc, token, signer.address(), task_bond).await?;
        if current_allowance < amount {
            let approve_calldata: Bytes = IERC20::approveCall {
                spender: task_bond,
                amount,
            }
            .abi_encode()
            .into();
            let approve_hash = sign_and_broadcast_tx(&rpc, &signer, token, approve_calldata)
                .await
                .map_err(|e| CmdError::Server(format!("approve broadcast failed: {e}")))?;
            let ok = rpc
                .wait_for_receipt(approve_hash, receipt_timeout, RECEIPT_POLL_INTERVAL)
                .await
                .map_err(|e| CmdError::Server(format!("approve receipt: {e}")))?;
            if !ok {
                return Err(CmdError::Server(format!(
                    "approve tx {approve_hash:#x} reverted on-chain"
                )));
            }
            approval_tx_hex = Some(format!("{approve_hash:#x}"));
        }
    }

    // 10. Broadcast TaskBond.post, wait for receipt.
    let post_tx = sign_and_broadcast_tx(&rpc, &signer, task_bond, post_calldata)
        .await
        .map_err(|e| CmdError::Server(format!("TaskBond.post broadcast failed: {e}")))?;
    let post_ok = rpc
        .wait_for_receipt(post_tx, receipt_timeout, RECEIPT_POLL_INTERVAL)
        .await
        .map_err(|e| CmdError::Server(format!("post receipt: {e}")))?;
    if !post_ok {
        return Err(CmdError::Server(format!(
            "TaskBond.post tx {post_tx:#x} reverted on-chain — report aborted"
        )));
    }
    let post_tx_hex = format!("{post_tx:#x}");

    // 11. Report the tx hash. The server enqueues `BondPosted` verification.
    let body = ReportStakePostTxBody {
        tx_hash: post_tx_hex.clone(),
        stake_source: args.source.to_report(),
    };
    let report = match client.inner().report_stake_post_tx(&task_id, &body).await {
        Ok(r) => r.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };

    // 12. Poll the quote until `bond_status=posted` (async verification). A
    //     timeout is not a failure — the tx is on-chain and confirmed.
    let verify_timeout = args.verify_timeout.unwrap_or(DEFAULT_VERIFY_TIMEOUT);
    let final_status = poll_until_posted(&client, &task_id, args.source, verify_timeout).await;

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": report.task_id,
            "status": report.status,
            "verification": report.verification,
            "bond_status": final_status,
            "post_tx_hash": post_tx_hex,
            "approval_tx_hash": approval_tx_hex,
            "token": format!("{token:#x}"),
            "amount": amount_i64,
            "task_ref": format!("{task_ref:#x}"),
            "salt": format!("{salt:#x}"),
        }),
    ))
}

/// Re-quote until `bond_status=posted` or `timeout` elapses. Returns the last
/// observed `bond_status` (`None` if the quote never carried one). Transient
/// quote errors are swallowed — the report already succeeded.
async fn poll_until_posted(
    client: &taskfast_client::TaskFastClient,
    task_id: &Uuid,
    source: BondStakeSource,
    timeout: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last: Option<String> = None;
    loop {
        if let Ok(v) = client
            .inner()
            .get_stake_quote(task_id, Some(source.to_quote()))
            .await
        {
            last = v.into_inner().bond_status;
            if last.as_deref() == Some("posted") {
                return last;
            }
        }
        if tokio::time::Instant::now() + VERIFY_POLL_INTERVAL >= deadline {
            return last;
        }
        tokio::time::sleep(VERIFY_POLL_INTERVAL).await;
    }
}

/// Resolve `(rpc_url, chain_id, default_stablecoin)` from `GET /config/network`
/// for this environment's network. Mirrors `post::resolve_rpc_url`'s override
/// handling and same-host proxy guard, but also surfaces `chain_id` and the
/// bond token address, which the bond flow needs from the same entry.
async fn resolve_network(
    ctx: &Ctx,
    client: &taskfast_client::TaskFastClient,
    args: &PostArgs,
) -> Result<(String, i64, Option<String>), CmdError> {
    let network = ctx.environment.network();
    let cfg = client.fetch_network_config().await.map_err(|e| match e {
        taskfast_client::Error::Auth(_) | taskfast_client::Error::Validation { .. } => {
            CmdError::Server(format!("fetch network config from {}: {e}", ctx.base_url()))
        }
        other => CmdError::from(other),
    })?;
    let name = network.as_str();
    let entry = cfg.entry(name).map_err(|e| {
        CmdError::Server(format!(
            "deployment at {} does not advertise network `{name}`: {e}",
            ctx.base_url()
        ))
    })?;

    let rpc_url = if let Some(ref override_url) = args.rpc_url {
        // Validate against the env-derived network policy (as post/escrow do),
        // not the advertised chain_id — the env is the trust anchor.
        validate_override_rpc_url(override_url, network, ctx.allow_custom_endpoints)?;
        override_url.clone()
    } else {
        // Same-host guard: the proxy may be mounted at any path on api_base, but
        // must live on the same host so the API key never leaves the deployment.
        if !is_proxy_rpc_url(&entry.rpc_url, ctx.base_url()) {
            return Err(CmdError::Server(format!(
                "deployment at {} returned rpc_url {:?} for network `{name}`, which is not on \
                 the same host. Refusing to route RPC traffic off-host.",
                ctx.base_url(),
                entry.rpc_url,
            )));
        }
        entry.rpc_url.clone()
    };
    Ok((rpc_url, entry.chain_id, entry.default_stablecoin.clone()))
}

// ponytail: these three ERC-20 read helpers are copied verbatim from
// `escrow.rs` (which keeps them private). Duplicating ~30 trivial lines is a
// smaller, lower-risk diff than hoisting them into a shared module and
// touching the escrow path; hoist if a third caller appears.
async fn erc20_balance_of(
    rpc: &TempoRpcClient,
    token: Address,
    owner: Address,
) -> Result<U256, CmdError> {
    let calldata: Bytes = IERC20::balanceOfCall { account: owner }.abi_encode().into();
    let raw = rpc
        .eth_call(token, &calldata)
        .await
        .map_err(|e| CmdError::Server(format!("balanceOf rpc: {e}")))?;
    decode_u256(&raw, "balanceOf")
}

async fn erc20_allowance(
    rpc: &TempoRpcClient,
    token: Address,
    owner: Address,
    spender: Address,
) -> Result<U256, CmdError> {
    let calldata: Bytes = IERC20::allowanceCall { owner, spender }.abi_encode().into();
    let raw = rpc
        .eth_call(token, &calldata)
        .await
        .map_err(|e| CmdError::Server(format!("allowance rpc: {e}")))?;
    decode_u256(&raw, "allowance")
}

fn decode_u256(bytes: &[u8], label: &str) -> Result<U256, CmdError> {
    if bytes.len() < 32 {
        return Err(CmdError::Decode(format!(
            "{label} returned {} bytes, expected >=32",
            bytes.len()
        )));
    }
    Ok(U256::from_be_slice(&bytes[..32]))
}
