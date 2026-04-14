//! `taskfast platform` — read global platform configuration.
//!
//! Single verb `config` → `GET /platform/config`. Surface the
//! chain-agnostic server defaults (fee splits, review windows, supported
//! tokens, contract addresses) needed before composing EIP-712 payloads
//! locally.

use clap::{Parser, Subcommand};
use serde_json::json;

use super::{CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::map_api_error;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Fetch the platform configuration snapshot.
    Config,
}

#[derive(Debug, Parser)]
pub struct Args;

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Config => config(ctx).await,
    }
}

async fn config(ctx: &Ctx) -> CmdResult {
    let client = ctx.client()?;
    let resp = match client.inner().get_platform_config().await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "config": resp }),
    ))
}
