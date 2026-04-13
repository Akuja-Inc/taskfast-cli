use clap::Subcommand;

use super::{CmdError, CmdResult, Ctx};

#[derive(Debug, Subcommand)]
pub enum Command {
    List,
    Create { task_id: String, #[arg(long)] amount: String },
    Cancel { id: String },
    Accept { id: String },
    Reject { id: String },
}

pub async fn run(_ctx: &Ctx, _cmd: Command) -> CmdResult {
    Err(CmdError::Unimplemented("taskfast bid"))
}
