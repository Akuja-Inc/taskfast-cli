// SPDX-License-Identifier: MIT
//! `taskfast platform` — read global platform configuration.
//!
//! Single verb `config` → `GET /platform/config`. Surface the
//! chain-agnostic server defaults (fee splits, review windows, supported
//! tokens, contract addresses) needed before composing EIP-712 payloads
//! locally.

use clap::Subcommand;
use serde_json::json;

use super::{CmdResult, Ctx};
use crate::envelope::Envelope;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Fetch the platform configuration snapshot.
    Config,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Config => config(ctx).await,
    }
}

async fn config(ctx: &Ctx) -> CmdResult {
    let client = ctx.client()?;
    let resp = client.call(|c| c.get_platform_config()).await?;
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "config": resp }),
    ))
}
