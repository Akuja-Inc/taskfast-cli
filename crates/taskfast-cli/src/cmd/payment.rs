// SPDX-License-Identifier: MIT
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
    pub from: Option<jiff::civil::Date>,

    /// ISO-8601 date (inclusive) upper bound.
    #[arg(long)]
    pub to: Option<jiff::civil::Date>,

    #[arg(long)]
    pub cursor: Option<String>,

    /// Max items per page. Defaults to 50 — earnings ledger pulls
    /// typically scan a wider window than active worker queues.
    #[arg(long, default_value_t = 50)]
    pub limit: i64,
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
    let resp = client.call(|c| c.get_task_payment(&task_id)).await?;
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
    let resp = client
        .call(|c| {
            c.list_agent_payments(
                args.cursor.as_deref(),
                args.from.as_ref(),
                taskfast_client::page_limit(args.limit),
                args.status.map(Into::into),
                args.to.as_ref(),
            )
        })
        .await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_parses_iso_dates_for_from_and_to() {
        let args = ListArgs::try_parse_from([
            "payment-list",
            "--from",
            "2026-03-01",
            "--to",
            "2026-03-31",
        ])
        .expect("ISO dates must parse");
        assert_eq!(args.from.map(|d| d.to_string()), Some("2026-03-01".into()));
        assert_eq!(args.to.map(|d| d.to_string()), Some("2026-03-31".into()));
    }

    #[test]
    fn list_rejects_non_iso_date() {
        assert!(
            ListArgs::try_parse_from(["payment-list", "--from", "March 1"]).is_err(),
            "non-ISO date must be refused"
        );
    }
}
