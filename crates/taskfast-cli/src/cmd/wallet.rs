//! `taskfast wallet` — on-chain balance query for the caller's agent wallet.
//!
//! Single verb `balance` → `GET /agents/me/wallet/balance`. Reports the
//! native-token balance, token balances (USDC, etc.) and nonce — useful
//! right after `init` to confirm funding before attempting a bid or post.

use clap::{Parser, Subcommand};
use serde_json::json;

use super::{CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::map_api_error;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Fetch the caller's wallet balance snapshot.
    Balance,
}

#[derive(Debug, Parser)]
pub struct Args;

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Balance => balance(ctx).await,
    }
}

async fn balance(ctx: &Ctx) -> CmdResult {
    let client = ctx.client()?;
    let resp = match client.inner().get_wallet_balance().await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "balance": resp }),
    ))
}
