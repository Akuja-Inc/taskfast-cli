//! `cargo xtask <cmd>` — repo automation entrypoint.
//!
//! Today: `sync-spec` is a stub. It will (per am-74l) load spec/openapi.yaml,
//! apply the in-memory error-schema normalizer, and hand the result to
//! progenitor during the taskfast-client build.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "TaskFast SDK repo automation.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Normalize + diff the OpenAPI spec used for client codegen.
    SyncSpec,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::SyncSpec => {
            eprintln!("xtask sync-spec: not yet implemented (tracked in am-74l)");
            std::process::exit(70);
        }
    }
}
