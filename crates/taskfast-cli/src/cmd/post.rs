use clap::Parser;

use super::{CmdError, CmdResult, Ctx};

#[derive(Debug, Parser)]
pub struct Args {
    #[arg(long)]
    pub title: String,
    #[arg(long)]
    pub budget: String,
    #[arg(long, value_delimiter = ',')]
    pub capabilities: Vec<String>,
    #[arg(long)]
    pub criteria: Option<String>,
}

pub async fn run(_ctx: &Ctx, _args: Args) -> CmdResult {
    Err(CmdError::Unimplemented("taskfast post"))
}
