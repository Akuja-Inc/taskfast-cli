// SPDX-License-Identifier: MIT
//! `taskfast events` — lifecycle event read/stream/ack + AsyncAPI schema.
//!
//! Subcommands:
//!
//! * [`poll`] — one-shot `GET /api/agents/me/events` (cursor-paginated
//!   read). Envelope-wrapped like every other subcommand.
//! * [`stream`] — long-running WebSocket to `/socket/agent` that emits
//!   one JSONL line per event on stdout. Bypasses the envelope — stdout
//!   is the contract. `--ack-stdin` acks events on the same socket,
//!   which advances the server-side cursor and releases the next event.
//! * [`ack`] — retired. The server has no HTTP ack route; the command
//!   fails with a pointer to `events stream --ack-stdin`.
//! * [`schema`] — `GET /api/asyncapi.json`. Prints the full AsyncAPI
//!   2.6 document or filters to a single event type via `--event`.

use clap::Subcommand;

use super::{CmdError, CmdResult, Ctx};

pub mod ack;
pub mod poll;
pub mod schema;
pub mod stream;

pub use ack::AckArgs;
pub use poll::PollArgs;
pub use schema::SchemaArgs;
pub use stream::StreamArgs;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// GET /agents/me/events — one page of lifecycle events.
    Poll(PollArgs),
    /// Stream lifecycle events over WebSocket as JSONL on stdout.
    ///
    /// Bypasses the JSON envelope: every line is either an event
    /// payload or a sentinel (`{"sentinel":"connected"}`,
    /// `{"sentinel":"reconnecting"}`, `{"sentinel":"resumed"}`,
    /// `{"sentinel":"closing"}`, `{"sentinel":"acked",…}`). Pass
    /// `--ack-stdin` and write each event id to stdin once handled;
    /// the server holds later events until the current one is acked.
    Stream(StreamArgs),
    /// Unsupported: the server has no HTTP ack route. Use
    /// `events stream --ack-stdin`.
    Ack(AckArgs),
    /// GET /asyncapi.json — AsyncAPI 2.6 schema for the event stream.
    Schema(SchemaArgs),
}

/// Dispatch non-stream subcommands. `Stream` is handled in `main.rs`
/// because it writes JSONL directly to stdout and must bypass the
/// envelope wrapper.
pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Poll(a) => poll::run(ctx, a).await,
        Command::Ack(a) => ack::run(a),
        Command::Schema(a) => schema::run(ctx, a).await,
        Command::Stream(_) => Err(CmdError::Usage(
            "`events stream` must be dispatched by main.rs (bypass envelope)".into(),
        )),
    }
}
