// SPDX-License-Identifier: MIT
//! `taskfast events ack <id>` — retired; points callers at
//! `events stream --ack-stdin`.
//!
//! The server has no HTTP ack route: acks are accepted only as an `ack`
//! message on the stream's own WebSocket, and a second socket would
//! disconnect the stream (one socket per agent). See [`super::stream`].

use clap::Parser;

use super::super::{CmdError, CmdResult};

#[derive(Debug, Parser)]
pub struct AckArgs {
    /// Event id from the live stream (UUID).
    pub event_id: String,
}

pub fn run(args: AckArgs) -> CmdResult {
    Err(CmdError::Usage(format!(
        "`events ack` is not supported: the server only accepts acks on the \
         event stream's own socket. Run `taskfast events stream --ack-stdin` \
         and write `{}` to its stdin after handling the event.",
        args.event_id
    )))
}
