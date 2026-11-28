// SPDX-License-Identifier: MIT
//! `taskfast bond post <task_id>` — auction-task operator posts the on-chain
//! performance bond in one step (gh#95).
//!
//! This command drives the bond flow for an auction task end to end:
//!
//!  1. `GET /tasks/:id/stake/quote` → `required_amount` (bond base units).
//!  2. Resolve the bond ERC-20 token, chain, and TaskBond contract from
//!     `GET /config/network` (`default_stablecoin`, `task_bond_contract`).
//!  3. Derive `taskRef` (16 zero bytes ++ task UUID) + a random `salt`.
//!  4. ERC-20 `allowance` preflight; `approve` the TaskBond contract if short.
//!  5. `TaskBond.post(token, amount, taskRef, salt)` — broadcast + wait receipt.
//!  6. `POST /tasks/:id/stake/report {tx_hash}` — a claim, not proof.
//!  7. Poll the quote until `bond_status=posted` (server verifies `BondPosted`
//!     asynchronously).
//!
//! Token and TaskBond contract both come from the deployment's network config
//! (`--token`/`--task-bond` override for custom-endpoint development). The
//! quote does not yet return `task_ref`/`salt`, so `taskRef` is derived
//! locally per the verifier's layout. The verifier matches
//! contract/token/taskRef/amount and does not pin salt, so a random salt is
//! accepted.
//!
//! Error mapping delegates to `map_api_error` (401|403→Auth, 409|422→
//! Validation). Keystore / signing failures surface as `Wallet` (exit 5).
//!
//! Every chain touch goes through [`ChainConnection`]: the deployment's entry
//! for the environment's network supplies the token, the TaskBond contract and
//! the proxy URL (same-host guarded); the node's `eth_chainId` must match the
//! entry's advertised chain; the keystore unlocks behind the same seam.

use std::path::PathBuf;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use clap::{Parser, ValueEnum};
use serde_json::json;
use uuid::Uuid;

use super::{parse_duration, CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;
use crate::tempo_session::{ChainConnection, ChainTarget, WalletOpts};

use taskfast_agent::chain::{compute_task_ref, TaskBond};
use taskfast_client::api::types::{
    GetStakeQuoteStakeSource, ReportStakePostTxBody, ReportStakePostTxBodyStakeSource,
};
use taskfast_client::map_api_error;

/// Server-side `BondPosted` verification is asynchronous, so after reporting we
/// re-quote until `bond_status=posted`. Bounded so the command still returns if
/// verification lags; a timeout is reported as `posting`, not a failure.
const DEFAULT_VERIFY_TIMEOUT: Duration = Duration::from_mins(1);
const VERIFY_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// clap-friendly mirror of the generated stake-source enums (kept local so
/// codegen needn't grow clap derives). kebab-case
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

    /// TaskBond contract address (`0x`-prefixed). Defaults to the deployment's
    /// `task_bond_contract` from `GET /config/network`; pass this only to
    /// override (e.g. custom-endpoint development).
    #[arg(long, env = "BOND_ADDRESS")]
    pub task_bond: Option<String>,

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

    /// Skip the on-chain `allowance` preflight + `approve` tx (the balance
    /// preflight still runs). Only safe when a sufficient allowance was already
    /// granted to the TaskBond contract.
    #[arg(long)]
    pub skip_allowance_check: bool,

    /// Receipt-polling ceiling — human duration (`3min`, `90s`). Falls back to
    /// `receipt_timeout` in config, then a chain-aware default.
    #[arg(long, env = "TASKFAST_RECEIPT_TIMEOUT", value_parser = parse_duration)]
    pub receipt_timeout: Option<Duration>,

    /// How long to poll the quote for `bond_status=posted` after reporting.
    /// A timeout returns `posting` (the tx is on-chain; verification is async),
    /// not an error.
    #[arg(long, env = "TASKFAST_BOND_VERIFY_TIMEOUT", value_parser = parse_duration)]
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

    // 4. Parse an explicit TaskBond override up front so a typo fails before
    //    any signing or network round-trip; the server-advertised default is
    //    resolved after the network-config fetch (step 6).
    let task_bond_override: Option<Address> = args
        .task_bond
        .as_deref()
        .map(|tb| {
            tb.parse().map_err(|e| {
                CmdError::Usage(format!(
                    "--task-bond `{tb}` is not a valid EVM address: {e}"
                ))
            })
        })
        .transpose()?;

    // 5 + 6. Resolve chain + token + contracts + RPC endpoint from the
    //    deployment's network config (through the chain-session seam), and —
    //    live only — refuse a node on another chain than the entry advertises.
    let wallet = WalletOpts {
        keystore: args.keystore.as_deref(),
        password_file: args.wallet_password_file.as_deref(),
        wallet_address: args.wallet_address.as_deref(),
        rpc_url: args.rpc_url.as_deref(),
        receipt_timeout: args.receipt_timeout,
    };
    let (conn, chain_id, default_stablecoin, task_bond_contract) =
        resolve_network(ctx, &client, &args).await?;
    let rpc_url = conn.rpc_url().to_string();
    let task_bond: Address = match task_bond_override {
        Some(a) => a,
        None => {
            let tb = task_bond_contract.ok_or_else(|| {
                CmdError::Server(
                    "deployment does not advertise a TaskBond contract \
                     (`task_bond_contract` in /config/network) for this network; \
                     pass --task-bond to override"
                        .into(),
                )
            })?;
            tb.parse().map_err(|e| {
                CmdError::Server(format!(
                    "deployment task_bond_contract `{tb}` is not a valid EVM address: {e}"
                ))
            })?
        }
    };
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

    // Flags and deployment fields are validated first, so a mistyped
    // --token or a missing contract fails before the wallet is opened or
    // the node is asked anything. Unlocks the wallet (validated under
    // --dry-run too), then verifies the node's chain id unless --dry-run.
    let session = conn.into_session(ctx, &wallet, "bond post").await?;

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

    // Balance preflight always runs — independent of --skip-allowance-check,
    // which governs only the allowance + approve path — so an underfunded
    // poster fails fast before broadcasting anything.
    let balance = session.erc20_balance_of(token, session.address()).await?;
    if balance < amount {
        return Err(CmdError::Usage(format!(
            "poster balance {balance} < required bond {amount} on token {token:#x}"
        )));
    }

    let mut approval_tx_hex: Option<String> = None;
    if !args.skip_allowance_check {
        approval_tx_hex = session
            .ensure_allowance(token, task_bond, amount)
            .await?
            .map(|h| format!("{h:#x}"));
    }

    // 10. Broadcast TaskBond.post, wait for receipt.
    let post_tx = session
        .send_transaction_and_wait(
            task_bond,
            post_calldata,
            "TaskBond.post",
            Some(" — report aborted"),
        )
        .await?;
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

/// The deployment's `GET /config/network` entry for this environment's
/// network, through the chain-session seam (override guard or same-host proxy).
async fn resolve_network(
    ctx: &Ctx,
    client: &taskfast_client::TaskFastClient,
    args: &PostArgs,
) -> Result<(ChainConnection, i64, Option<String>, Option<String>), CmdError> {
    let network = ctx.environment.network();
    let conn = ChainConnection::resolve(
        ctx,
        client,
        args.rpc_url.as_deref(),
        ChainTarget::Network(network),
    )
    .await?;
    // A `Network` target always selects an entry.
    let entry = conn.entry().cloned().ok_or_else(|| {
        CmdError::Server(
            "deployment does not advertise a network entry for this environment; pass \
             --task-bond and --token to override"
                .into(),
        )
    })?;
    Ok((
        conn,
        entry.chain_id,
        entry.default_stablecoin.clone(),
        entry.task_bond_contract.clone(),
    ))
}
