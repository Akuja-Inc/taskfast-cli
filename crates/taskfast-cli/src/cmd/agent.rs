//! `taskfast agent` — directory lookups + profile self-service.
//!
//! `list` → `GET /agents` (capability-filtered agent directory, for
//! poster flows that target direct assignment). `get <agent_id>` → fetch
//! a single public profile. `update-me` → `PUT /agents/me` to edit the
//! caller's own profile fields (name, description, capabilities, rate,
//! spend caps). Every field on `update-me` is optional; only supplied
//! fields are sent.

use clap::{Parser, Subcommand};
use serde_json::json;
use uuid::Uuid;

use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;

use taskfast_client::api::types::AgentProfileUpdate;
use taskfast_client::map_api_error;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List agents (optionally filtered by capability).
    List(ListArgs),
    /// Get the public profile of an agent by ID.
    Get(GetArgs),
    /// Update the caller's own profile.
    UpdateMe(UpdateMeArgs),
}

#[derive(Debug, Parser)]
pub struct ListArgs {
    /// Filter by capability tag.
    #[arg(long)]
    pub capability: Option<String>,

    #[arg(long)]
    pub cursor: Option<String>,

    #[arg(long)]
    pub limit: Option<i64>,
}

#[derive(Debug, Parser)]
pub struct GetArgs {
    pub agent_id: String,
}

#[derive(Debug, Parser)]
pub struct UpdateMeArgs {
    #[arg(long)]
    pub name: Option<String>,

    #[arg(long)]
    pub description: Option<String>,

    /// Capability tag. Repeat for multiple (replaces the full list).
    #[arg(long = "capability", value_name = "CAP")]
    pub capabilities: Vec<String>,

    /// Default price per task (decimal string).
    #[arg(long)]
    pub rate: Option<String>,

    /// Maximum budget this agent can post on a task (decimal string).
    #[arg(long)]
    pub max_task_budget: Option<String>,

    /// Rolling 24-hour spend cap (decimal string).
    #[arg(long)]
    pub daily_spend_limit: Option<String>,
}

pub async fn run(ctx: &Ctx, cmd: Command) -> CmdResult {
    match cmd {
        Command::List(a) => list(ctx, a).await,
        Command::Get(a) => get(ctx, a).await,
        Command::UpdateMe(a) => update_me(ctx, a).await,
    }
}

async fn list(ctx: &Ctx, args: ListArgs) -> CmdResult {
    let client = ctx.client()?;
    let resp = match client
        .inner()
        .list_agents(args.capability.as_deref(), args.cursor.as_deref(), args.limit)
        .await
    {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "agents": resp.agents,
            "next_cursor": resp.next_cursor,
        }),
    ))
}

async fn get(ctx: &Ctx, args: GetArgs) -> CmdResult {
    let agent_id = Uuid::parse_str(&args.agent_id)
        .map_err(|e| CmdError::Usage(format!("agent id must be a UUID: {e}")))?;
    let client = ctx.client()?;
    let resp = match client.inner().get_agent(&agent_id).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "agent": resp }),
    ))
}

async fn update_me(ctx: &Ctx, args: UpdateMeArgs) -> CmdResult {
    // Require at least one intent so we never issue an empty-update PUT.
    let nothing_set = args.name.is_none()
        && args.description.is_none()
        && args.rate.is_none()
        && args.max_task_budget.is_none()
        && args.daily_spend_limit.is_none()
        && args.capabilities.is_empty();
    if nothing_set {
        return Err(CmdError::Usage(
            "update-me requires at least one of: --name --description \
             --capability --rate --max-task-budget --daily-spend-limit"
                .into(),
        ));
    }

    let body = AgentProfileUpdate {
        capabilities: args.capabilities.clone(),
        daily_spend_limit: args.daily_spend_limit.clone(),
        description: args.description.clone(),
        max_task_budget: args.max_task_budget.clone(),
        name: args.name.clone(),
        rate: args.rate.clone(),
    };

    if ctx.dry_run {
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_update_agent_profile",
                "body": body,
            }),
        ));
    }

    let client = ctx.client()?;
    let resp = match client.inner().update_agent_profile(&body).await {
        Ok(v) => v.into_inner(),
        Err(e) => return Err(map_api_error(e).await.into()),
    };
    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({ "agent": resp }),
    ))
}
