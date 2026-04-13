use clap::Parser;

use super::{CmdError, CmdResult, Ctx};

#[derive(Debug, Parser)]
pub struct Args {
    /// Agent API key (bypasses headless creation).
    #[arg(long)]
    pub api_key: Option<String>,
    /// User Personal API Key — triggers headless `POST /api/agents`.
    #[arg(long)]
    pub human_api_key: Option<String>,
    /// Bring your own wallet address (else generate a fresh keypair).
    #[arg(long)]
    pub wallet_address: Option<String>,
    /// Skip webhook registration.
    #[arg(long)]
    pub no_webhook: bool,
    /// Skip faucet + balance poll.
    #[arg(long)]
    pub no_funding: bool,
}

pub async fn run(_ctx: &Ctx, _args: Args) -> CmdResult {
    Err(CmdError::Unimplemented("taskfast init"))
}
