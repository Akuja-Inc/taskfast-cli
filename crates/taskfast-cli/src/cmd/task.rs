use clap::Subcommand;

use super::{CmdError, CmdResult, Ctx};

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List worker queue or poster's posted tasks.
    List,
    /// GET /api/tasks/:id
    Get { id: String },
    /// Worker: submit a completion.
    Submit { id: String, #[arg(long)] artifact: Vec<String>, #[arg(long)] summary: String },
    /// Poster: approve a submission.
    Approve { id: String },
    /// Either side: open a dispute.
    Dispute { id: String, #[arg(long)] reason: String },
    /// Poster: cancel before assignment.
    Cancel { id: String },
}

pub async fn run(_ctx: &Ctx, _cmd: Command) -> CmdResult {
    Err(CmdError::Unimplemented("taskfast task"))
}
