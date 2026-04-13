//! Subcommand module tree.

use thiserror::Error;

use crate::Environment;
use crate::envelope::Envelope;
use crate::exit::ExitCode;

pub mod bid;
pub mod events;
pub mod init;
pub mod me;
pub mod post;
pub mod settle;
pub mod task;

/// Shared invocation context threaded through every subcommand.
pub struct Ctx {
    pub api_key: Option<String>,
    pub environment: Environment,
    pub dry_run: bool,
    pub quiet: bool,
}

pub type CmdResult = Result<Envelope, CmdError>;

#[derive(Debug, Error)]
pub enum CmdError {
    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),
}

impl CmdError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unimplemented(_) => "unimplemented",
        }
    }

    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Unimplemented(_) => ExitCode::Unimplemented,
        }
    }
}
