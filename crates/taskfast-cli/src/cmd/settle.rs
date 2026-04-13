use clap::Parser;

use super::{CmdError, CmdResult, Ctx};

#[derive(Debug, Parser)]
pub struct Args {
    pub task_id: String,
}

pub async fn run(_ctx: &Ctx, _args: Args) -> CmdResult {
    Err(CmdError::Unimplemented("taskfast settle"))
}
