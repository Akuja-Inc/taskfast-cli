use clap::Parser;

use super::{CmdError, CmdResult, Ctx};

#[derive(Debug, Parser)]
pub struct Args {
    /// Reconstruct in-flight tasks/bids state.
    #[arg(long)]
    pub resume: bool,
}

pub async fn run(_ctx: &Ctx, _args: Args) -> CmdResult {
    Err(CmdError::Unimplemented("taskfast me"))
}
