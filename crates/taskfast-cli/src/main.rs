//! `taskfast` binary entry point.
//!
//! Phase 1 scaffold: clap command tree in place, all subcommands exit with
//! the `unimplemented` envelope. Bodies land in follow-up tasks.

use clap::{Parser, Subcommand};

mod cmd;
mod envelope;
mod exit;

use envelope::Envelope;
use exit::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "taskfast",
    version,
    about = "TaskFast marketplace CLI — worker + poster hot loop."
)]
struct Cli {
    /// API key (overrides TASKFAST_API_KEY env).
    #[arg(long, global = true, env = "TASKFAST_API_KEY")]
    api_key: Option<String>,

    /// Target environment.
    #[arg(long, global = true, default_value = "prod", env = "TASKFAST_ENV")]
    env: Environment,

    /// Short-circuit mutations; reads pass through.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Emit tracing logs to stderr.
    #[arg(long, global = true, value_name = "LEVEL", num_args = 0..=1, default_missing_value = "info")]
    verbose: Option<String>,

    /// Suppress even the error envelope (exit code still conveys outcome).
    #[arg(long, global = true, conflicts_with = "verbose")]
    quiet: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Environment {
    Prod,
    Staging,
    Local,
}

impl Environment {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prod => "production",
            Self::Staging => "staging",
            Self::Local => "local",
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Bootstrap an agent: deps, wallet, webhook, funding.
    Init(cmd::init::Args),
    /// Profile + readiness (GET /agents/me + /agents/me/readiness).
    Me(cmd::me::Args),
    /// Task operations (list, get, submit, approve, dispute, cancel).
    #[command(subcommand)]
    Task(cmd::task::Command),
    /// Bid operations (list, create, cancel, accept, reject).
    #[command(subcommand)]
    Bid(cmd::bid::Command),
    /// Poster: create a task (two-phase draft + sign + submit).
    Post(cmd::post::Args),
    /// Poster: sign a DistributionApproval and settle a task.
    Settle(cmd::settle::Args),
    /// Event polling (stream as JSON-lines).
    #[command(subcommand)]
    Events(cmd::events::Command),
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    if let Some(level) = cli.verbose.as_deref() {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(level)
            .init();
    }

    let ctx = cmd::Ctx {
        api_key: cli.api_key,
        environment: cli.env,
        dry_run: cli.dry_run,
        quiet: cli.quiet,
    };

    let result = match cli.command {
        Command::Init(a) => cmd::init::run(&ctx, a).await,
        Command::Me(a) => cmd::me::run(&ctx, a).await,
        Command::Task(c) => cmd::task::run(&ctx, c).await,
        Command::Bid(c) => cmd::bid::run(&ctx, c).await,
        Command::Post(a) => cmd::post::run(&ctx, a).await,
        Command::Settle(a) => cmd::settle::run(&ctx, a).await,
        Command::Events(c) => cmd::events::run(&ctx, c).await,
    };

    match result {
        Ok(env) => {
            if !cli.quiet {
                env.emit();
            }
            ExitCode::Success.into()
        }
        Err(e) => {
            if !cli.quiet {
                Envelope::error(ctx.environment, ctx.dry_run, &e).emit();
            }
            e.exit_code().into()
        }
    }
}
