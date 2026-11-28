// SPDX-License-Identifier: MIT
//! `taskfast post` — two-phase task creation (prepare → sign+broadcast → submit).
//!
//! Supersedes the `scripts/post-task` shell flow whose defect chain is tracked
//! under am-q1m. Server contract (`lib/task_fast/payments/tempo_provider.ex`
//! `charge_submission_fee/4`) accepts **two voucher forms** on
//! `POST /api/task_drafts/{id}/submit`:
//!
//!   - `0x<64hex>` → already-broadcast tx hash; server polls for confirmation.
//!   - any other hex → raw RLP-encoded signed tx; server broadcasts via
//!     `eth_sendRawTransaction`.
//!
//! We take the **tx-hash voucher path**: the client signs + broadcasts the
//! ERC-20 transfer locally, then hands the server just the hash. Rationale:
//! failures on the broadcast side surface to the agent as a concrete RPC
//! error before we pollute the server's draft state with a would-be-bad
//! voucher.
//!
//! `payload_to_sign` (despite its historical name) is **not** an ECDSA signing
//! payload — it's the encoded ERC-20 `transfer` calldata. We wrap it in a
//! replay-safe legacy tx and broadcast via
//! [`TempoSession::send_fee_transfer`](crate::tempo_session::TempoSession::send_fee_transfer),
//! behind the chain-session seam (endpoint guards, node chain id, wallet).

use std::path::PathBuf;

use alloy_primitives::{Address, Bytes};
use clap::Parser;
use serde_json::json;

use super::wallet_args::decode_0x_bytes;
use super::{CmdError, CmdResult, Ctx};
use crate::envelope::Envelope;
use crate::tempo_session::validate_override_rpc_url;
use crate::tempo_session::{ChainConnection, ChainTarget, WalletOpts};

use taskfast_client::api::types::{
    CompletionCriterionInput, TaskDraftPrepareRequest, TaskDraftPrepareRequestAssignmentType,
    TaskDraftPrepareRequestPickupDeadlineHours, TaskDraftSubmitRequest,
    TaskDraftSubmitRequestSignature,
};

#[derive(Debug, Parser)]
pub struct Args {
    /// Task title (required).
    #[arg(long)]
    pub title: String,

    /// Task description (required, non-empty). Validated client-side so a
    /// typoed script fails fast instead of paying for a server 422 round-trip.
    #[arg(long)]
    pub description: String,

    /// Max budget the poster will pay, as a decimal string ("2.50"). Passed
    /// through as `budget_max` on the draft.
    #[arg(long)]
    pub budget: Option<String>,

    /// Capability tags required from the assignee. Comma-separated.
    #[arg(long, value_delimiter = ',')]
    pub capabilities: Vec<String>,

    /// Completion-criterion payout gate as a JSON object. Repeat `--criterion`
    /// for multiple gates. Shape matches `CompletionCriterionInput`:
    /// `{"description":"…","check_type":"json_schema|regex|count|http_status|file_exists",`
    /// `"check_expression":"…","expected_value":"…","target_artifact_pattern":null}`.
    /// Missing ⇒ no objective gate; server policy decides payout.
    #[arg(long = "criterion")]
    pub criteria: Vec<String>,

    /// Path to a JSON file containing an array of `CompletionCriterionInput`
    /// objects. Merged before any `--criterion` flags (file entries first).
    /// Use this when you have many gates or want to keep them in version
    /// control alongside the task.
    #[arg(long)]
    pub criteria_file: Option<PathBuf>,

    /// Pickup window in hours: how long the task stays open for bidding
    /// before closing. Required. The server owns the set of allowed windows
    /// (`422 pickup_deadline_required` on an invalid choice).
    #[arg(long)]
    pub pickup_deadline_hours: i64,

    /// Execution deadline (RFC3339 timestamp).
    #[arg(long)]
    pub execution_deadline: Option<String>,

    /// Settlement venue `chain_key` (e.g. `tempo-zone-moderato`) electing the
    /// settlement rail/chain for this task (ADR 0011 §5). Omit to settle on the
    /// deployment's default venue (L1). The server validates the key (must
    /// reference an active venue) and binds it once at creation, immutably.
    #[arg(long)]
    pub venue: Option<String>,

    /// Poster's on-chain wallet address (0x-prefixed). Matches `TEMPO_WALLET_ADDRESS`
    /// written by `taskfast init`.
    #[arg(long, env = "TEMPO_WALLET_ADDRESS")]
    pub wallet_address: Option<String>,

    /// Keystore reference. Accepts a bare path (`/.../abc.json`) or the
    /// `file:/path` form `taskfast init` writes to `TEMPO_KEY_SOURCE`.
    /// Keychain backends will slot in later under the same flag.
    #[arg(long, env = "TEMPO_KEY_SOURCE")]
    pub keystore: Option<String>,

    /// Path to a file holding the keystore password (mode-0400 recommended).
    /// Preferred over `TASKFAST_WALLET_PASSWORD` — files don't leak via
    /// process args or `/proc/self/environ`.
    #[arg(long, env = "TASKFAST_WALLET_PASSWORD_FILE")]
    pub wallet_password_file: Option<PathBuf>,

    /// Tempo RPC endpoint. Defaults to the canonical proxy URL the
    /// deployment advertises for the env's network.
    #[arg(long, env = "TEMPO_RPC_URL")]
    pub rpc_url: Option<String>,

    /// Acknowledge oversized budgets. Required when the budget exceeds
    /// `confirm_above_budget` in the config. No-op when the gate is unset
    /// or when the budget is below it. Fail-closed by design (no TTY
    /// prompt) so a CI script accident doesn't broadcast a huge approve.
    #[arg(long)]
    pub yes: bool,
}

/// Convert `--pickup-deadline-hours` to the wire type generated from the
/// vendored spec. The generated newtype is the only encoding of the allowed
/// set — the CLI never restates it, so a server policy change flows in via a
/// spec re-vendor with no code edit here.
fn pickup_hours_to_wire(
    hours: i64,
) -> Result<TaskDraftPrepareRequestPickupDeadlineHours, CmdError> {
    TaskDraftPrepareRequestPickupDeadlineHours::try_from(hours).map_err(|_| CmdError::Validation {
        code: "pickup_deadline_invalid".into(),
        message: format!(
            "pickup window of {hours}h is not an allowed bidding window per the \
             API contract; the server owns the valid set (422 pickup_deadline_required)"
        ),
    })
}

pub async fn run(ctx: &Ctx, args: Args) -> CmdResult {
    // gh#144: validate every local input in ONE pass before any HTTP,
    // keystore prompt, or signing — the old sequential early-returns cost an
    // agent one full retry cycle per missing flag / guard conflict.
    let pre = preflight(ctx, &args)?;

    let prep_body = TaskDraftPrepareRequest {
        assignment_type: TaskDraftPrepareRequestAssignmentType::default(),
        budget_max: args.budget.clone(),
        completion_criteria: pre.completion_criteria,
        description: args.description.clone(),
        execution_deadline: pre.execution_deadline,
        pickup_deadline_hours: pre.pickup_hours,
        poster_wallet_address: pre
            .wallet_address
            .parse()
            .map_err(|e| CmdError::Usage(format!("--wallet-address rejected by schema: {e}")))?,
        required_capabilities: args.capabilities.clone(),
        settlement_venue: args.venue.clone(),
        title: args.title.clone(),
    };

    // Dry-run must perform zero HTTP. Predict the proxy URL locally so the
    // envelope still reports what the real path would have used.
    if ctx.dry_run {
        let rpc_url = match pre.rpc_url_override {
            Some(override_url) => override_url,
            None => format!(
                "{}/rpc/{}",
                ctx.base_url().trim_end_matches('/'),
                ctx.environment.network().as_str()
            ),
        };
        return Ok(Envelope::success(
            ctx.environment,
            ctx.dry_run,
            json!({
                "action": "would_post",
                "draft_id": serde_json::Value::Null,
                "title": args.title,
                "budget_max": args.budget,
                "settlement_venue": args.venue,
                "required_capabilities": args.capabilities,
                "completion_criteria_count": prep_body.completion_criteria.len(),
                "rpc_url": rpc_url,
                "wallet_address": pre.wallet_address,
            }),
        ));
    }

    let client = ctx.client()?;

    // Resolve the RPC endpoint through the chain-session seam. Default path:
    // the deployment's `GET /config/network` (public) proxy. Override path:
    // the user-supplied `--rpc-url` / `TEMPO_RPC_URL` (already checked in
    // preflight). No server-issued chain id here, so the node's own chain id
    // is what the fee guards key on.
    let wallet = WalletOpts {
        keystore: args.keystore.as_deref(),
        password_file: args.wallet_password_file.as_deref(),
        wallet_address: Some(pre.wallet_address.as_str()),
        rpc_url: pre.rpc_url_override.as_deref(),
        receipt_timeout: None,
    };
    let conn = ChainConnection::resolve(ctx, &client, wallet.rpc_url, ChainTarget::Node).await?;

    // Phase 1 — prepare. Server returns ERC-20 transfer calldata + draft_id.
    let prep = client.call(|c| c.prepare_task_draft(&prep_body)).await?;

    let calldata = decode_0x_bytes(&prep.payload_to_sign)
        .map_err(|e| CmdError::Server(format!("server returned invalid payload_to_sign: {e}")))?;

    // Unlock the keystore only after prepare succeeds — avoids prompting the
    // user for a keystore password on a request that never leaves local
    // validation. The node's chain id is pinned next: a `Node` target has no
    // server-issued chain to compare it against, and the fee guards key on it.
    let session = conn.into_session(ctx, &wallet, "submission fee").await?;
    let tx_hash = session
        .send_fee_transfer(ctx, &prep.token_address, Bytes::from(calldata))
        .await?;
    let tx_hash_hex = format!("{tx_hash:#x}");

    // Phase 2 — submit voucher. The field is named `signature` for historical
    // reasons; semantically it's a voucher (tx hash, in our path).
    let signature: TaskDraftSubmitRequestSignature = tx_hash_hex
        .parse()
        .map_err(|e| CmdError::Server(format!("tx hash rejected by schema pattern: {e}")))?;
    let submit_body = TaskDraftSubmitRequest { signature };
    let submitted = client
        .call(|c| c.submit_task_draft(&prep.draft_id, &submit_body))
        .await?;

    // gh#156: a task created at `blocked_on_submission_fee_debt` looks like a
    // stalled post unless the envelope says otherwise. The draft-submit
    // response carries no server `message` (the server's message exists only
    // on the direct `POST /tasks` create view), so when the task lands
    // blocked we mirror the server's canonical create message — the envelope
    // then distinguishes "waiting for the fee tx to confirm" from "post
    // failed". `submission_fee_status` above carries the machine state.
    let blocked = submitted.status == "blocked_on_submission_fee_debt";
    if blocked {
        // stderr audit line, same contract as the signing line above — a CI
        // log must show WHY the task is not `open` yet and how to watch it.
        eprintln!(
            "taskfast: task {} parked at blocked_on_submission_fee_debt — \
             submission-fee transfer confirming on-chain; it opens automatically \
             once confirmed. Poll `taskfast task get {}`.",
            submitted.id, submitted.id
        );
    }
    let message = blocked.then(|| {
        "Task created. Submission fee transaction is pending on-chain \
         confirmation before the task becomes actionable."
            .to_string()
    });

    Ok(Envelope::success(
        ctx.environment,
        ctx.dry_run,
        json!({
            "task_id": submitted.id,
            "draft_id": prep.draft_id,
            "submission_fee_tx_hash": tx_hash_hex,
            "status": submitted.status,
            "submission_fee_status": submitted.submission_fee_status,
            "message": message,
        }),
    ))
}

/// Everything [`run`] needs from local validation, parsed once by
/// [`preflight`] so the runtime path never re-validates (gh#144).
struct Preflight {
    wallet_address: String,
    execution_deadline: Option<jiff::Timestamp>,
    completion_criteria: Vec<CompletionCriterionInput>,
    pickup_hours: TaskDraftPrepareRequestPickupDeadlineHours,
    /// User-supplied `--rpc-url` / `TEMPO_RPC_URL`, already through the
    /// custom-endpoint guard. `None` = resolve the deployment's proxy URL.
    rpc_url_override: Option<String>,
}

/// Bare problem text for the preflight list — `Display` on these variants
/// carries an `usage:` / `validation [code]:` prefix that reads as noise
/// inside an enumerated list (the envelope's `error.code` already classifies).
fn problem_text(e: &CmdError) -> String {
    match e {
        CmdError::Usage(m) => m.clone(),
        CmdError::Validation { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// gh#144 one-pass input validation. Every locally-checkable problem with
/// the invocation — missing/blank required values, malformed addresses and
/// UUIDs, bad timestamps, unparseable criteria, out-of-contract pickup
/// windows, endpoint-guard conflicts, the budget gate — is collected and
/// reported in a single enumerated `Usage` error, so fixing one problem
/// never reveals exactly one more. Runs before any HTTP, keystore prompt,
/// or signing, and short-circuits `--dry-run` the same way.
///
/// Error message texts are unchanged from the former sequential checks —
/// orchestrators scraping them see the same strings, just together.
fn preflight(ctx: &Ctx, args: &Args) -> Result<Preflight, CmdError> {
    let mut problems: Vec<String> = Vec::new();
    // Each check stores one validated field as `Option`; `None` marks
    // "check failed, problem already recorded" so every later check still
    // runs and reports too.

    if args.description.trim().is_empty() {
        problems.push(
            "--description must not be empty (server requires a non-blank description)".into(),
        );
    }

    if let Err(e) = ctx.enforce_budget_gate(args.budget.as_deref(), args.yes, "post a task") {
        problems.push(problem_text(&e));
    }

    let wallet_address = match args
        .wallet_address
        .as_deref()
        .or(ctx.wallet_address.as_deref())
    {
        Some(w) => match w.parse::<Address>() {
            Ok(_) => Some(w.to_string()),
            Err(e) => {
                problems.push(format!("--wallet-address is not a valid EVM address: {e}"));
                None
            }
        },
        None => {
            problems.push(
                "--wallet-address (or TEMPO_WALLET_ADDRESS, or wallet_address in config) required to post a task".into(),
            );
            None
        }
    };

    let execution_deadline =
        match parse_iso_opt(args.execution_deadline.as_deref(), "--execution-deadline") {
            Ok(v) => Some(v),
            Err(e) => {
                problems.push(problem_text(&e));
                None
            }
        };

    let completion_criteria = match resolve_criteria(args.criteria_file.as_deref(), &args.criteria)
    {
        Ok(v) => Some(v),
        Err(e) => {
            problems.push(problem_text(&e));
            None
        }
    };

    let pickup_hours = match pickup_hours_to_wire(args.pickup_deadline_hours) {
        Ok(w) => Some(w),
        Err(e) => {
            problems.push(problem_text(&e));
            None
        }
    };

    let rpc_url_override = match args.rpc_url.as_deref() {
        Some(u) => match validate_override_rpc_url(
            u,
            ctx.environment.network(),
            ctx.allow_custom_endpoints,
        ) {
            Ok(u) => Some(Some(u.to_string())),
            Err(e) => {
                problems.push(problem_text(&e));
                None
            }
        },
        None => Some(None),
    };

    if !problems.is_empty() {
        let n = problems.len();
        let list = problems
            .iter()
            .enumerate()
            .map(|(i, p)| format!("  {}. {p}", i + 1))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(CmdError::Usage(format!(
            "post preflight found {n} problem{}:\n{list}",
            if n == 1 { "" } else { "s" }
        )));
    }

    Ok(Preflight {
        wallet_address: wallet_address.expect("no problems ⇒ wallet present"),
        execution_deadline: execution_deadline.expect("no problems ⇒ deadline resolved"),
        completion_criteria: completion_criteria.expect("no problems ⇒ criteria resolved"),
        pickup_hours: pickup_hours.expect("no problems ⇒ pickup hours valid"),
        rpc_url_override: rpc_url_override.expect("no problems ⇒ rpc override resolved"),
    })
}

/// Merge file-sourced and inline `--criterion` payloads into one validated
/// list of `CompletionCriterionInput`. File entries are prepended so a
/// shared base file can be augmented with one-off overrides on the command
/// line. Any shape/parse failure is a `Usage` error — the CLI catches it
/// before the request ever reaches the server, so the exit-code contract
/// (Auth=re-credential, Validation=server-rejected payload) stays clean.
fn resolve_criteria(
    file: Option<&std::path::Path>,
    inline: &[String],
) -> Result<Vec<CompletionCriterionInput>, CmdError> {
    let mut out = Vec::with_capacity(inline.len());
    if let Some(path) = file {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            CmdError::Usage(format!("read --criteria-file {}: {e}", path.display()))
        })?;
        let parsed: Vec<CompletionCriterionInput> = serde_json::from_str(&raw).map_err(|e| {
            CmdError::Usage(format!(
                "--criteria-file {} is not a JSON array of CompletionCriterionInput: {e}",
                path.display()
            ))
        })?;
        out.extend(parsed);
    }
    for (i, raw) in inline.iter().enumerate() {
        let one: CompletionCriterionInput = serde_json::from_str(raw)
            .map_err(|e| CmdError::Usage(format!("--criterion[{i}] not valid JSON: {e}")))?;
        out.push(one);
    }
    Ok(out)
}

fn parse_iso_opt(s: Option<&str>, flag: &str) -> Result<Option<jiff::Timestamp>, CmdError> {
    match s {
        None => Ok(None),
        Some(raw) => raw
            .parse::<jiff::Timestamp>()
            .map(Some)
            .map_err(|e| CmdError::Usage(format!("{flag} not a valid RFC3339 timestamp: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Network;

    #[test]
    fn validate_override_rejects_without_opt_in() {
        let err = validate_override_rpc_url("https://mallory.example", Network::Mainnet, false)
            .expect_err("override without opt-in must be refused");
        match err {
            CmdError::Usage(msg) => {
                assert!(msg.contains("--allow-custom-endpoints"), "msg: {msg}");
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn validate_override_accepts_with_opt_in() {
        validate_override_rpc_url("https://my-node.example", Network::Testnet, true)
            .expect("opt-in bypasses the guard");
    }

    #[test]
    fn validate_override_rejects_plain_http_on_mainnet() {
        let err = validate_override_rpc_url("http://my-node.example", Network::Mainnet, true)
            .expect_err("plain-http on mainnet is always refused (except loopback)");
        assert!(matches!(err, CmdError::Usage(msg) if msg.contains("plain-HTTP")));
    }

    #[test]
    fn validate_override_allows_plain_http_loopback_on_mainnet() {
        validate_override_rpc_url("http://127.0.0.1:8545", Network::Mainnet, true)
            .expect("loopback bypasses mainnet-HTTPS check");
        validate_override_rpc_url("http://localhost:8545", Network::Mainnet, true)
            .expect("loopback bypasses mainnet-HTTPS check");
    }

    #[test]
    fn validate_override_allows_plain_http_on_testnet() {
        validate_override_rpc_url("http://my-testnet.example", Network::Testnet, true)
            .expect("testnet does not enforce HTTPS");
    }

    #[test]
    fn parse_iso_accepts_rfc3339_deadline() {
        let got = parse_iso_opt(Some("2026-03-01T12:00:00Z"), "--execution-deadline").unwrap();
        assert_eq!(
            got.map(|d| d.to_string()),
            Some("2026-03-01T12:00:00Z".into())
        );
    }

    #[test]
    fn parse_iso_rejects_non_rfc3339_and_passes_none_through() {
        assert!(parse_iso_opt(Some("tomorrow"), "--execution-deadline").is_err());
        assert!(parse_iso_opt(None, "--execution-deadline")
            .unwrap()
            .is_none());
    }

    #[test]
    fn pickup_hours_conversion_follows_api_contract() {
        // The allowed set lives only in the vendored spec (via the generated
        // newtype) — an out-of-set value maps to a validation error without
        // the CLI restating the list anywhere.
        let err = pickup_hours_to_wire(3).expect_err("3 is outside the contract set");
        assert!(matches!(err, CmdError::Validation { .. }));
        // A contract-valid choice converts to the exact wire integer.
        let wire = pickup_hours_to_wire(24).expect("24 is contract-valid");
        assert_eq!(i64::from(wire), 24);
    }

    #[test]
    fn network_name_matches_config_map_keys() {
        // The lookup key into `GET /config/network`'s `networks` map.
        // Must stay in sync with what the deployment advertises.
        assert_eq!(Network::Mainnet.as_str(), "mainnet");
        assert_eq!(Network::Testnet.as_str(), "testnet");
    }
}
