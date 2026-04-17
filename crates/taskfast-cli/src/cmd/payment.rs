//! `taskfast payment` — task-scoped escrow state and agent-wide earnings.
//!
//! `get <task_id>` → `GET /tasks/{id}/payment` (escrow breakdown for a task).
//! `list` → `GET /agents/me/payments` (paginated earnings ledger with
//! optional status + date-range filters, used to reconcile on-chain events).

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::api::types::ListAgentPaymentsStatus;
use taskfast_client::map_api_error;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Get the escrow breakdown for a specific task.
    Get(GetArgs),
    /// List the caller's payment history (earnings ledger).
    List(ListArgs),
}

#[derive(Debug, Parser)]
pub struct GetArgs {
    pub task_id: String,
}

#[derive(Debug, Parser)]
pub struct ListArgs {
    #[arg(long)]
    pub status: Option<PaymentStatusFilter>,

    /// ISO-8601 date (inclusive) lower bound.
    #[arg(long)]
    pub from: Option<chrono::NaiveDate>,

    /// ISO-8601 date (inclusive) upper bound.
    #[arg(long)]
    pub to: Option<chrono::NaiveDate>,

    #[arg(long)]
    pub cursor: Option<String>,

    #[arg(long)]
    pub limit: Option<i64>,
}

/// clap-friendly mirror of `ListAgentPaymentsStatus`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PaymentStatusFilter {
    Pending,
    Disbursed,
    Held,
    Failed,
}

impl From<PaymentStatusFilter> for ListAgentPaymentsStatus {
    fn from(s: PaymentStatusFilter) -> Self {
        match s {
            PaymentStatusFilter::Pending => Self::Pending,
            PaymentStatusFilter::Disbursed => Self::Disbursed,
            PaymentStatusFilter::Held => Self::Held,
            PaymentStatusFilter::Failed => Self::Failed,
        }
    }
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::Get(a) => get(ctx, a).await,
        Command::List(a) => list(ctx, a).await,
    }
}

async fn get(ctx: &Ctx, args: GetArgs) -> CmdResult {
    let task_id = Uuid::parse_str(&args.task_id)
        .map_err(|e| CmdError::Usage(format!("task id must be a UUID: {e}")))?;
    let client = ctx.client()?;
    let resp = match client.inner().get_task_payment(&task_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": task_id.to_string(),
            "payment": resp,
        }),
    ))
}

async fn list(ctx: &Ctx, args: ListArgs) -> CmdResult {
    let client = ctx.client()?;
    let resp = match client
        .inner()
        .list_agent_payments(
            args.cursor.as_deref(),
            args.from.as_ref(),
            args.limit,
            args.status.map(Into::into),
            args.to.as_ref(),
        )
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "payments": resp.data,
            "summary": resp.summary,
            "meta": resp.meta,
        }),
    ))
}
