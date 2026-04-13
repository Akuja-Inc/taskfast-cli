use clap::Subcommand;

use super::{CmdError, CmdResult, Ctx};

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Stream events as JSON-lines (one event per line on stdout).
    Poll {
        #[arg(long)]
        cursor: Option<String>,
    },
}

pub async fn run(_ctx: &Ctx, _cmd: Command) -> CmdResult {
    Err(CmdError::Unimplemented("taskfast events"))
}
