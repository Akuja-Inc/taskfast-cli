// SPDX-License-Identifier: MIT
//! `taskfast init` — the onboarding command.
//!
//! Replaces `init.sh`'s step 1-9 orchestration with a non-interactive,
//! CLI-driven flow. Every input comes from a flag, an env var, or the
//! existing [`Config`] file — there are no TTY prompts, because the
//! caller is expected to be another agent/LLM.
//!
//! # Scope (this slice, am-yvc)
//!
//! * api_key: direct via `--api-key` / `TASKFAST_API_KEY` / config file.
//! * validate: `GET /agents/me` — must be active.
//! * readiness: `GET /agents/me/readiness` — informs wallet gate.
//! * wallet: BYOW via `--wallet-address`, or generate + keystore with a
//!   password sourced from `--wallet-password-file` / `TASKFAST_WALLET_PASSWORD`.
//! * config file: load + write at `ctx.config_path` (default
//!   `./.taskfast/config.json`, chmod 600 on unix).
//! * final readiness assert.
//!
//! # Scope (am-z58 extension)
//!
//! * `--human-api-key` (+ optional agent-* fields): when no agent key is
//!   available, POST /agents with the user PAT to mint one. The server
//!   derives `owner_id` from the PAT, so the CLI never asks the caller for
//!   it. The minted `api_key` is then used for the rest of the flow and
//!   written to the config file. Under `--dry-run` the mint is skipped and the
//!   envelope reports `agent.action = "would_mint"` — the rest of the flow
//!   is also skipped because the real agent key never materialized.
//!
//! # Scope (am-iit extension)
//!
//! * Optional `--webhook-url` (+ `--webhook-secret-file`, repeat
//!   `--webhook-event`) folds webhook registration into the init run:
//!   PUT /agents/me/webhooks, persist the returned secret (chmod 600),
//!   and optionally replace the subscription list. Skipped silently
//!   when the URL isn't supplied so callers who prefer a separate
//!   `taskfast webhook register` step still get the pre-am-iit flow.
//!
//! Deferred so this slice stays reviewable: balance polling after faucet dispense.
//!
//! # Scope (gh#142 extension)
//!
//! * End-of-init funding sanity check: a failed `--fund` faucet request
//!   or a zero wallet balance prints a prominent stderr warning with the
//!   manual fallback (`cast rpc tempo_fundAddress …` against the
//!   deployment's advertised rpc_url, or <https://wallet.tempo.xyz>) and
//!   the server's `funding_hint` when the readiness `funded` gate emits
//!   one. The run stays `ok:true` — the gap is a warning, not a failure.
//!
//! # Scope (gh#238 extension)
//!
//! * No key of any kind: `POST /new` on the configured host, with no
//!   authentication. The server creates an unclaimed agent, its key and
//!   a claim link in one call; the key then drives the rest of the flow
//!   like a PAT-minted one. The agent belongs to nobody until a person
//!   opens the link, cannot accept a bid until then, and its key stops
//!   working at the `expires_at` the server returns (72 hours today), so
//!   the link and the time are printed and saved next to the key in the
//!   config file the moment the 201 arrives — before anything else can
//!   fail — and later put in the envelope. Order of resort when no key is
//!   resolvable: `--human-api-key` → interactive PAT prompt (TTY only) →
//!   `POST /new`; a PAT wins because that agent is claimed from the
//!   start. The call is made exactly once — the server counts every
//!   request against a per-address daily limit — after everything that
//!   would deterministically fail later (name, description, wallet flags)
//!   has been checked, and its three refusals (404 switched off, 429 daily
//!   limit, 503 Terms not published) end the run with a plain message and
//!   nothing written.
//!
//! # `--dry-run` semantics
//!
//! Mutations short-circuit: no wallet POST, no config file write, no
//! keystore write. A wallet is still generated (so the address is real)
//! but its signer is dropped at the end of the function. Readiness and
//! profile reads pass through.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use clap::Parser;
use serde_json::json;

use super::{CmdError, CmdResult, Ctx};
use crate::config::Config;
use crate::envelope::Envelope;

use alloy_signer_local::PrivateKeySigner;
use taskfast_agent::bootstrap::{create_agent_headless, get_readiness, validate_auth};
use taskfast_agent::faucet::{request_testnet_funds, FaucetDrop};
use taskfast_agent::keystore;
use taskfast_agent::wallet;
use taskfast_client::api::types::{
    AgentCreateRequest, AgentReadiness, NewAgentRequest, NewAgentRequestDescription,
    NewAgentRequestName,
};
use taskfast_client::{NewAgentOutcome, TaskFastClient};

/// Wallet status string emitted by the server when the agent hasn't
/// registered one yet. `AgentReadinessChecks.wallet.status == "complete"`
/// means it's already done.
const WALLET_STATUS_COMPLETE: &str = "complete";

#[derive(Debug, Parser)]
// CLI surface: each flag is an independent user opt-in/out. Collapsing
// them into a state machine or nested enum would make clap's derive
// macro unergonomic without narrowing the actual accepted inputs.
#[allow(clippy::struct_excessive_bools)]
pub struct Args {
    /// Wallet address to register (BYOW). Mutually exclusive with
    /// `--generate-wallet`.
    #[arg(long, conflicts_with = "generate_wallet")]
    pub wallet_address: Option<String>,

    /// Generate a fresh keypair, persist it via the keystore module, then
    /// register the derived address with TaskFast.
    #[arg(long)]
    pub generate_wallet: bool,

    /// Path to a file containing the keystore password. Required when
    /// `--generate-wallet` is used without `TASKFAST_WALLET_PASSWORD` set.
    /// Prefer a mode-0400 file over `--wallet-password` (which leaks via
    /// process args).
    #[arg(long, env = "TASKFAST_WALLET_PASSWORD_FILE")]
    pub wallet_password_file: Option<PathBuf>,

    /// Explicit keystore path override. Default: XDG data dir +
    /// `<address>.json`.
    #[arg(long)]
    pub keystore_path: Option<PathBuf>,

    /// Skip wallet provisioning entirely. Useful for workers that never
    /// settle (rare) or for rebuilding config state without touching chain.
    #[arg(long)]
    pub skip_wallet: bool,

    /// Opt in to the testnet faucet for a freshly-generated wallet on
    /// testnet environments (Staging / Local). Off by default so prod
    /// scripts (and CI flows supplying funds out-of-band) never
    /// accidentally hit the faucet. No-op on Prod — mainnet is never
    /// auto-funded.
    #[arg(long)]
    pub fund: bool,

    /// User PAT (`tf_user_*`) used to headlessly mint a fresh agent via
    /// `POST /agents` when no agent API key is available. The server
    /// derives the owning user from the PAT, so no owner UUID is required.
    /// Ignored if an agent key is already resolvable (direct flag / env
    /// var / config file). Preferred over the no-credential path: with a
    /// PAT the agent is claimed from the start, whereas `POST /new`
    /// creates one a person must claim before the `expires_at` it returns.
    #[arg(long, env = "TASKFAST_HUMAN_API_KEY")]
    pub human_api_key: Option<String>,

    /// Display name for the agent created without a key — from a PAT or
    /// through `POST /new`. 1 to 80 characters.
    #[arg(long, default_value = "taskfast-agent")]
    pub agent_name: String,

    /// Description for the agent created without a key — from a PAT or
    /// through `POST /new`. 1 to 500 characters.
    #[arg(long, default_value = "Headless agent registered via taskfast init")]
    pub agent_description: String,

    /// Capability tag for the agent minted from a PAT (repeat to pass
    /// multiple). If none are provided, defaults to `["general"]` so the
    /// request still satisfies the non-empty-array convention most
    /// consumers expect. Ignored for an agent created through `POST /new`
    /// with no key — the server gives it no capabilities until a person
    /// claims it — and init then says so on stderr.
    #[arg(long = "agent-capability", value_name = "CAP")]
    pub agent_capabilities: Vec<String>,

    /// HTTPS URL to register for webhook event delivery. When supplied,
    /// init PUTs `/agents/me/webhooks` after the wallet step and (if
    /// the server returns one) persists the signing secret to
    /// `--webhook-secret-file`. Omitting this flag leaves webhooks
    /// untouched — `taskfast webhook register` can configure them later.
    #[arg(long)]
    pub webhook_url: Option<String>,

    /// Path to persist the webhook signing secret (chmod 600 on unix).
    /// The platform returns the secret exactly once at creation time
    /// — a fresh re-register with an existing config returns `null`
    /// and the existing file is left untouched.
    #[arg(long)]
    pub webhook_secret_file: Option<PathBuf>,

    /// Event type to subscribe to after webhook registration. Repeat
    /// for multiple. Ignored unless `--webhook-url` is also set. When
    /// `--webhook-url` is supplied and this flag is omitted, init
    /// auto-subscribes to the worker default set (same list as
    /// `taskfast webhook subscribe --default-events`); pass
    /// `--no-default-events` to opt out and register a URL-only endpoint.
    #[arg(long = "webhook-event", value_name = "EVENT")]
    pub webhook_events: Vec<String>,

    /// Opt out of auto-subscribing to default worker events when
    /// `--webhook-url` is set without any explicit `--webhook-event`.
    /// Preserves the pre-existing behavior of registering a URL with
    /// zero subscriptions (a deliberate URL-only push endpoint).
    #[arg(long)]
    pub no_default_events: bool,

    /// Disable all TTY prompts even when stdin is a terminal. Use in
    /// scripts or pipes where interactive fallback would stall, and to
    /// reach the no-key `POST /new` path at a terminal — otherwise init
    /// first asks for a PAT. Env: `TASKFAST_NO_INTERACTIVE=1`.
    #[arg(long, env = "TASKFAST_NO_INTERACTIVE")]
    pub no_interactive: bool,

    /// Inline keystore password populated by the interactive wallet-
    /// generate prompt. Never exposed as a CLI arg (would leak via
    /// process args). Takes precedence over `--wallet-password-file`
    /// when set, so the prompted value round-trips into the normal
    /// password-resolution path.
    #[arg(skip)]
    pub inline_wallet_password: Option<String>,
}

pub async fn run(ctx: &Ctx, args: Args) -> CmdResult {
    let interactive = !args.no_interactive && crate::cmd::init_tui::is_interactive();
    run_with_prompter(
        ctx,
        args,
        &crate::cmd::init_tui::DialoguerPrompter,
        interactive,
    )
    .await
}

/// Same as [`run`] but with an injectable [`crate::cmd::init_tui::Prompter`] and an explicit
/// `interactive` gate so tests can drive the TUI branches under
/// `cargo test` (where neither stdin nor stdout is a TTY). Production
/// callers should use [`run`].
pub async fn run_with_prompter<P: crate::cmd::init_tui::Prompter>(
    ctx: &Ctx,
    mut args: Args,
    prompter: &P,
    interactive: bool,
) -> CmdResult {
    let cfg_path = ctx.config_path.clone();

    // 1. Load any existing config so re-running init is idempotent. A
    //    config-supplied api_key is layered under the CLI/env sources
    //    Ctx already resolved (flag > env var > config).
    let mut cfg = Config::load(&cfg_path)?;
    // The key as it was on disk when the run started, for step 4's check
    // that a claim link saved earlier still belongs to the key in use.
    let config_api_key_on_start = cfg.api_key.clone();

    // 1a. Resolve the agent API key. If none is available, mint one:
    //     from a user PAT when the caller supplied one (or, on a TTY, typed
    //     one in), else through the no-credential `POST /new`. Under
    //     --dry-run both short-circuit before any POST and return early —
    //     the rest of the flow depends on a real key.
    //
    //     A PAT goes first on purpose: its agent is claimed from the start,
    //     while a `POST /new` agent needs a person to open the claim link
    //     within 72 hours. The interactive prompt is TTY-only, so a script
    //     with no key at all goes straight to `POST /new`.
    let (api_key, agent_outcome) = match resolve_api_key(ctx, &cfg) {
        Ok(k) => (k, AgentOutcome::PreExisting),
        Err(CmdError::MissingApiKey) => {
            let minted = match resolve_pat(ctx, &args, interactive, prompter).await {
                Ok(pat) => mint_agent(ctx, &args, &pat).await?,
                // No PAT and no prompt possible: last resort, no login.
                Err(CmdError::MissingApiKey) => create_unclaimed(ctx, &args).await?,
                Err(e) => return Err(e),
            };
            match minted {
                MintedAgent::DryRun { ref intent } => {
                    return Ok(Envelope::success(
                        ctx.environment,
                        ctx.dry_run,
                        build_dry_run_mint_envelope(&cfg_path, intent),
                    ));
                }
                MintedAgent::Live { ref api_key, .. } => {
                    (api_key.clone(), AgentOutcome::Minted(minted))
                }
            }
        }
        Err(e) => return Err(e),
    };

    // 1a'. An unclaimed agent exists the moment `POST /new` answered 201,
    //      and its key and claim link exist nowhere else. Save both before
    //      anything else can fail and tell the human now (gh#238 review,
    //      P1): a later error — a 5xx on /agents/me, a refused wallet
    //      registration — then leaves a usable config behind, and a re-run
    //      finds the key there and continues instead of minting another
    //      agent against the same daily quota. Refusals of `POST /new`
    //      itself never reach this point, so they still write nothing.
    //
    //      Said before it is saved: the link is the one thing a person
    //      needs to claim the agent, and a save can fail (an unwritable
    //      working directory is enough) — the note must not go down with
    //      it.
    if let AgentOutcome::Minted(MintedAgent::Live {
        id,
        unclaimed: Some(unclaimed),
        ..
    }) = &agent_outcome
    {
        if let Some(note) = unclaimed_announcement(&agent_outcome) {
            eprintln!("{note}");
        }
        cfg.environment = Some(ctx.environment);
        cfg.api_key = Some(api_key.clone());
        if let Some(id) = id {
            cfg.agent_id = Some(id.clone());
        }
        cfg.claim_url = unclaimed.claim_url.clone();
        cfg.claim_expires_at = unclaimed.expires_at.map(|t| t.to_string());
        cfg.save(&cfg_path)?;
    }

    // 1b. Interactive wallet-mode prompt. Only fires when this run just
    //     minted a fresh agent AND the human gave us zero wallet signal
    //     on the command line. Re-init against an existing agent key is
    //     treated as silent (preserves `init_idempotent_on_reinit`) so
    //     humans re-running `taskfast init` to refresh state don't get
    //     re-prompted about wallet setup they already completed.
    if interactive
        && matches!(agent_outcome, AgentOutcome::Minted(_))
        && args.wallet_address.is_none()
        && !args.generate_wallet
        && !args.skip_wallet
    {
        apply_wallet_mode_prompt(&mut args, prompter)?;
    }

    let effective_ctx = Ctx {
        api_key: Some(api_key.clone()),
        ..ctx.clone()
    };
    let client = effective_ctx.client()?;

    // 2. Validate auth + fetch readiness.
    //    On Auth failure with a PreExisting key, rewrite the error with a
    //    remediation hint — the common cause is a stale api_key left over in
    //    the config from a prior init run whose agent was deleted server-side.
    let profile = match validate_auth(&client).await {
        Ok(p) => p,
        Err(taskfast_client::Error::Auth(msg))
            if matches!(agent_outcome, AgentOutcome::PreExisting) =>
        {
            return Err(CmdError::Auth(stale_key_hint(
                &msg, &cfg_path, &cfg, ctx, &args,
            )));
        }
        Err(e) => return Err(CmdError::from(e)),
    };
    assert_active(&profile)?;
    // Cross-system invariant — fail-closed before any wallet/agent state
    // is written if the deployment advertises a different (or additional)
    // network than the env mandates. Skipped under --dry-run so a `taskfast
    // init --dry-run` still succeeds against a fresh deployment. One
    // exception, deliberate (gh#238): a key just minted through `POST /new`
    // is already on disk by now (step 1a'), because that key exists
    // server-side and is shown once — discarding it is worse than a config
    // the operator must delete after a strict-mode failure, and the error
    // says so.
    if !ctx.dry_run {
        super::enforce_server_network_invariant(&effective_ctx, &client).await?;
    }
    let readiness = get_readiness(&client).await.map_err(CmdError::from)?;

    // 3. Wallet provisioning.
    let wallet_outcome = if args.skip_wallet {
        WalletOutcome::Skipped
    } else if readiness.checks.wallet.status == WALLET_STATUS_COMPLETE
        && args.wallet_address.is_none()
        && !args.generate_wallet
    {
        // Nothing to do — server already has a wallet and caller isn't
        // forcing a new one.
        WalletOutcome::AlreadyConfigured
    } else {
        provision_wallet(&client, &args, ctx.dry_run).await?
    };

    // 4. Update the config in-memory (always — writing is gated by dry-run).
    //    `api_base` and `network` are no longer persisted — both are derived
    //    from `environment` at runtime via `Environment::api_base` /
    //    `Environment::network`.
    cfg.environment = Some(ctx.environment);
    cfg.api_key = Some(api_key.clone());
    if let Some(addr) = wallet_outcome.address() {
        cfg.wallet_address = Some(addr.to_string());
    }
    if let Some(path) = wallet_outcome.keystore_path() {
        cfg.keystore_path = Some(path.to_path_buf());
    }
    if let AgentOutcome::Minted(MintedAgent::Live { id: Some(id), .. }) = &agent_outcome {
        cfg.agent_id = Some(id.clone());
    }
    match &agent_outcome {
        // gh#238: the claim link is the only way a person can take the
        // agent over, so it lives next to the key that needs claiming.
        AgentOutcome::Minted(MintedAgent::Live {
            unclaimed: Some(unclaimed),
            ..
        }) => {
            cfg.claim_url = unclaimed.claim_url.clone();
            cfg.claim_expires_at = unclaimed.expires_at.map(|t| t.to_string());
        }
        // Any other fresh agent makes the previous link stale.
        AgentOutcome::Minted(_) => {
            cfg.claim_url = None;
            cfg.claim_expires_at = None;
        }
        // gh#238 review: a key from a flag or env var can be a different
        // agent than the one whose link the config carries. The field's
        // promise is "`None` for every other agent", so the link goes
        // unless the key in use is the one that was on disk.
        AgentOutcome::PreExisting => {
            if config_api_key_on_start.as_deref() != Some(api_key.as_str()) {
                cfg.claim_url = None;
                cfg.claim_expires_at = None;
            }
        }
    }
    if let Some(url) = args.webhook_url.as_deref().filter(|s| !s.trim().is_empty()) {
        cfg.webhook_url = Some(url.to_string());
    }
    if let Some(path) = args.webhook_secret_file.as_ref() {
        cfg.webhook_secret_path = Some(path.clone());
    }

    let config_written = if ctx.dry_run {
        false
    } else {
        cfg.save(&cfg_path)?;
        true
    };

    // 4b. Testnet funding. When a wallet was just generated on `--network
    //     testnet`, hit the public Tempo moderato faucet. Fire-and-surface:
    //     we report the faucet's tx hashes in the envelope but don't block
    //     on confirmation here. Caller-side balance polling lives under
    //     am-c74.
    //
    //     Mainnet is deliberately skipped — production wallets must be
    //     funded by the owning human at `https://wallet.tempo.xyz`. The
    //     envelope surfaces this with `status: "skipped", reason: "mainnet"`
    //     plus a `funding_hint` so orchestrators can tell the human where
    //     to go.
    let faucet_outcome = maybe_request_faucet(
        &args,
        ctx.environment.network(),
        &wallet_outcome,
        ctx.dry_run,
    )
    .await;

    // 4c. Optional webhook registration. Only runs when `--webhook-url`
    //     is provided; otherwise the envelope reports `status: "skipped"`
    //     so orchestrators can tell the step ran and found nothing to do
    //     (vs. the CLI not supporting webhooks at all).
    let webhook_outcome = maybe_configure_webhook(&client, &args, ctx.dry_run).await;

    // 5. Final readiness check — surfaces any remaining gates (webhook,
    //    funding) the caller still has to clear.
    let final_readiness = get_readiness(&client).await.map_err(CmdError::from)?;

    // 5b. Funding sanity check (gh#142): a faucet failure or zero wallet
    //     balance gets a prominent stderr warning with the manual fallback,
    //     so it can't sit quietly next to a successful envelope.
    warn_if_unfunded(
        &client,
        ctx,
        cfg.wallet_address.as_deref(),
        &faucet_outcome,
        &final_readiness,
    )
    .await;

    let data = build_envelope_data(
        &cfg_path,
        config_written,
        &wallet_outcome,
        &agent_outcome,
        &final_readiness,
        &faucet_outcome,
        &webhook_outcome,
        ctx.dry_run,
    );
    Ok(Envelope::success(ctx.environment, ctx.dry_run, data))
}

/// Result of the agent-key resolution step. When the caller already had
/// a key (flag / env var / config file), this is `PreExisting` and the
/// envelope stays quiet about it. When a key was minted without one —
/// from `--human-api-key` or through `POST /new` — the envelope surfaces
/// the new agent's id and name.
enum AgentOutcome {
    PreExisting,
    Minted(MintedAgent),
}

/// Live vs dry-run distinction for minting. Live carries the full
/// response; dry-run carries just the would-have-called payload so the
/// envelope can echo it back without touching the network.
#[cfg_attr(test, derive(Debug))]
enum MintedAgent {
    Live {
        api_key: String,
        id: Option<String>,
        name: Option<String>,
        /// Set when the agent came from `POST /new` (gh#238): nobody owns
        /// it until a person opens the claim link.
        unclaimed: Option<Unclaimed>,
    },
    DryRun {
        intent: MintIntent,
    },
}

/// The claim half of a `POST /new` response. Every field is optional
/// because the generated response type is (the spec marks them required
/// and the server sends them all); each consumer omits what is absent
/// rather than failing a run whose key already works.
#[derive(Clone)]
#[cfg_attr(test, derive(Debug))]
struct Unclaimed {
    /// Link a person opens to take the agent over.
    claim_url: Option<String>,
    /// When the agent's key stops working unless it is claimed first.
    expires_at: Option<jiff::Timestamp>,
    terms_url: Option<String>,
    terms_version: Option<String>,
    /// The server's own sentence stating that the request accepted the
    /// Terms — printed verbatim, never reworded here.
    terms_notice: Option<String>,
    terms_accepted: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MintKind {
    /// `POST /agents` with a user PAT: the agent belongs to that user now.
    FromPat,
    /// `POST /new` with no login: the agent waits to be claimed.
    Unclaimed,
}

impl MintKind {
    /// `data.agent.action` of the `--dry-run` envelope.
    fn dry_run_action(self) -> &'static str {
        match self {
            Self::FromPat => "would_mint",
            Self::Unclaimed => "would_mint_unclaimed",
        }
    }
}

#[cfg_attr(test, derive(Debug))]
struct MintIntent {
    kind: MintKind,
    name: String,
    description: String,
    capabilities: Vec<String>,
}

/// Resolve the PAT used to mint a fresh agent. Priority: `--human-api-key`
/// (flag / env) → interactive prompt (when TTY-attached). Absent both,
/// propagate [`CmdError::MissingApiKey`] so the caller falls through to
/// the no-credential `POST /new` (gh#238) — the same error non-interactive
/// callers saw before any of this existed.
async fn resolve_pat<P: crate::cmd::init_tui::Prompter>(
    ctx: &Ctx,
    args: &Args,
    interactive: bool,
    prompter: &P,
) -> Result<String, CmdError> {
    if let Some(p) = args.human_api_key.as_deref().filter(|s| !s.is_empty()) {
        return Ok(p.to_string());
    }
    if !interactive {
        return Err(CmdError::MissingApiKey);
    }
    let accounts_url = crate::accounts_url(ctx.base_url());
    let pat = prompter
        .pat(&accounts_url)
        .map_err(|e| CmdError::Usage(format!("PAT prompt failed: {e}")))?;
    // Greet via /users/me when available. 404 (endpoint not yet deployed)
    // falls back to a neutral confirmation — we never block the flow on
    // the greeting.
    let pat_client = TaskFastClient::from_api_key(ctx.base_url(), &pat).map_err(CmdError::from)?;
    let profile = pat_client.get_user_profile().await.ok().flatten();
    eprintln!("{}", crate::cmd::init_tui::greeting(profile.as_ref()));
    Ok(pat)
}

/// Drive the interactive wallet-mode prompt and fold the result back
/// into `args` so the existing `provision_wallet` branches see the same
/// state they would have after flag parsing.
fn apply_wallet_mode_prompt<P: crate::cmd::init_tui::Prompter>(
    args: &mut Args,
    prompter: &P,
) -> Result<(), CmdError> {
    use crate::cmd::init_tui::WalletMode;
    let mode = prompter
        .wallet_mode()
        .map_err(|e| CmdError::Usage(format!("wallet-mode prompt failed: {e}")))?;
    match mode {
        WalletMode::Byow => {
            let addr = prompter
                .wallet_address()
                .map_err(|e| CmdError::Usage(format!("wallet-address prompt failed: {e}")))?;
            args.wallet_address = Some(addr.trim().to_string());
        }
        WalletMode::Generate => {
            args.generate_wallet = true;
            let pw = prompter
                .wallet_password()
                .map_err(|e| CmdError::Usage(format!("password prompt failed: {e}")))?;
            args.inline_wallet_password = Some(pw);
        }
        WalletMode::Skip => {
            args.skip_wallet = true;
        }
    }
    Ok(())
}

async fn mint_agent(ctx: &Ctx, args: &Args, pat: &str) -> Result<MintedAgent, CmdError> {
    if pat.is_empty() {
        return Err(CmdError::Usage("PAT is empty".into()));
    }

    let capabilities = if args.agent_capabilities.is_empty() {
        vec!["general".to_string()]
    } else {
        args.agent_capabilities.clone()
    };

    if ctx.dry_run {
        return Ok(MintedAgent::DryRun {
            intent: MintIntent {
                kind: MintKind::FromPat,
                name: args.agent_name.clone(),
                description: args.agent_description.clone(),
                capabilities,
            },
        });
    }

    let pat_client = TaskFastClient::from_api_key(ctx.base_url(), pat).map_err(CmdError::from)?;
    let body = AgentCreateRequest {
        owner_id: None,
        name: args.agent_name.clone(),
        description: args.agent_description.clone(),
        capabilities,
        rate: None,
        max_task_budget: None,
        daily_spend_limit: None,
        payout_method: None,
        payment_method: None,
        tempo_wallet_address: None,
    };
    let resp = create_agent_headless(&pat_client, &body)
        .await
        .map_err(CmdError::from)?;
    let api_key = resp
        .api_key
        .clone()
        .ok_or_else(|| CmdError::Server("POST /agents returned no api_key despite 201".into()))?;
    Ok(MintedAgent::Live {
        api_key,
        id: resp.id.map(|u| u.to_string()),
        name: resp.name.clone(),
        unclaimed: None,
    })
}

/// Create an agent with no credential at all: `POST /new` (gh#238). The
/// server answers with the key, the claim link and the time the key
/// stops working; the flow then continues exactly as for a PAT-minted
/// agent. The request accepts the agent Terms on the caller's behalf —
/// that is the server's published rule, and the response's `terms_notice`
/// is surfaced verbatim so nobody has to guess at it.
///
/// Made at most once per run and never retried: the server counts every
/// request against a per-address daily quota, refused ones included.
/// Everything that would deterministically fail later is checked here
/// first, so a bad flag never spends that quota on an agent the run then
/// abandons: the name and description against the spec's own length
/// limits (the generated types carry them), and the wallet step's two
/// requirements (see [`preflight_wallet_args`]).
async fn create_unclaimed(ctx: &Ctx, args: &Args) -> Result<MintedAgent, CmdError> {
    // The generated types carry the spec's length limits; the server also
    // rejects a value that is blank after trimming, and only after its
    // rate limiter has counted the request — so that rule is mirrored here
    // too, next to the length checks (gh#238 review).
    if args.agent_name.trim().is_empty() {
        return Err(CmdError::Usage("--agent-name: must not be blank".into()));
    }
    if args.agent_description.trim().is_empty() {
        return Err(CmdError::Usage(
            "--agent-description: must not be blank".into(),
        ));
    }
    let name = NewAgentRequestName::from_str(&args.agent_name)
        .map_err(|e| CmdError::Usage(format!("--agent-name: {e}")))?;
    let description = NewAgentRequestDescription::from_str(&args.agent_description)
        .map_err(|e| CmdError::Usage(format!("--agent-description: {e}")))?;
    preflight_wallet_args(args)?;
    if !args.agent_capabilities.is_empty() {
        eprintln!("{}", capabilities_ignored_note(&args.agent_capabilities));
    }

    if ctx.dry_run {
        return Ok(MintedAgent::DryRun {
            intent: MintIntent {
                kind: MintKind::Unclaimed,
                name: args.agent_name.clone(),
                description: args.agent_description.clone(),
                // `POST /new` takes no capabilities; the server sets none.
                capabilities: Vec::new(),
            },
        });
    }

    let client = TaskFastClient::anonymous(ctx.base_url()).map_err(CmdError::from)?;
    let body = NewAgentRequest {
        name: Some(name),
        description: Some(description),
    };
    // The client has already decided, on the raw response, which of the
    // server's documented refusals this is; everything else is a generic
    // error with its usual mapping.
    let resp = match client.create_unclaimed_agent(&body).await {
        Ok(NewAgentOutcome::Created(resp)) => *resp,
        Ok(refused) => return Err(new_agent_refused(ctx, refused)),
        Err(e) => return Err(CmdError::from(e)),
    };

    let api_key = resp
        .api_key
        .clone()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| CmdError::Server("POST /new returned no api_key despite 201".into()))?;
    Ok(MintedAgent::Live {
        api_key,
        id: resp.agent_id.map(|u| u.to_string()),
        name: Some(args.agent_name.clone()),
        unclaimed: Some(Unclaimed {
            claim_url: resp.claim_url,
            expires_at: resp.expires_at,
            terms_url: resp.terms_url,
            terms_version: resp.terms_version,
            terms_notice: resp.terms_notice,
            terms_accepted: resp.terms_accepted,
        }),
    })
}

/// The wallet step's one requirement on the flags, shared by
/// [`preflight_wallet_args`] and [`provision_wallet`] so the two can't
/// drift apart.
const WALLET_FLAG_HINT: &str =
    "pass --wallet-address <0x...> or --generate-wallet (or --skip-wallet to defer)";

/// Fail the two ways [`provision_wallet`] would fail later, before the one
/// request that creates an agent (gh#238 review, P1). A fresh agent never
/// has a wallet, so the run must carry a wallet signal; and
/// `--generate-wallet` must be able to find a keystore password. Without
/// this, `taskfast init` with no wallet flag — the path runs
/// non-interactively, so nothing prompts — would create an agent and then
/// drop its key and claim link on the usage error; the user fixes the flag,
/// re-runs, and a second agent is minted against the same daily quota.
fn preflight_wallet_args(args: &Args) -> Result<(), CmdError> {
    if args.wallet_address.is_none() && !args.generate_wallet && !args.skip_wallet {
        return Err(CmdError::Usage(WALLET_FLAG_HINT.into()));
    }
    if args.generate_wallet {
        // Only the lookup matters here; `provision_wallet` resolves it
        // again when it actually needs the value.
        resolve_wallet_password(args)?;
    }
    Ok(())
}

/// stderr note for `--agent-capability` on the `POST /new` path (gh#238
/// review, P3): the server creates an unclaimed agent with no
/// capabilities, and `PUT /agents/me` answers 403 `claim_required` until a
/// person claims it, so the flags can't take effect now or be applied
/// later by the CLI.
fn capabilities_ignored_note(capabilities: &[String]) -> String {
    format!(
        "WARNING: --agent-capability is ignored for an agent created through POST /new: {} \
         was asked for, but the server gives an unclaimed agent no capabilities until a \
         person claims it. Set them after claiming with `taskfast agent update-me`.",
        capabilities
            .iter()
            .map(|c| format!("--agent-capability {c}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Why `POST /new` refused to create an agent. The three cases the server
/// documents are expected outcomes rather than faults, so each maps to
/// the plain message an orchestrator can branch on and a human can read.
/// The client already told them apart on the raw response
/// ([`NewAgentOutcome`]); nothing here parses an error's `Display`.
#[derive(Debug, PartialEq, Eq)]
enum NewAgentRefusal {
    /// 404: the deployment has the no-login route switched off.
    Off,
    /// 429 with the server's `rate_limited` envelope: this address used up
    /// its daily quota; `Retry-After` says when the window closes (up to
    /// a day).
    DailyLimit { retry_after: std::time::Duration },
    /// 503 with the server's `try_later` envelope: the agent Terms are not
    /// published yet. Carries the server's own sentence explaining it.
    NotReady(String),
}

impl NewAgentRefusal {
    /// The refusal the client reported. `None` for `Created`, which the
    /// caller has already matched before reaching the refusal path.
    fn from_outcome(outcome: &NewAgentOutcome) -> Option<Self> {
        match outcome {
            NewAgentOutcome::RouteOff => Some(Self::Off),
            NewAgentOutcome::RateLimited { retry_after } => Some(Self::DailyLimit {
                retry_after: *retry_after,
            }),
            NewAgentOutcome::NotReady { message } => Some(Self::NotReady(message.clone())),
            NewAgentOutcome::Created(_) => None,
        }
    }

    /// The error `init` returns. Nothing has been written at this point
    /// in the run, so every arm saves nothing by construction.
    ///
    /// `Off` keeps the exact error and exit code a keyless run always
    /// had: the caller needs a key, and existing orchestrators already
    /// branch on `missing_api_key` for it. `DailyLimit` keeps
    /// `retry_after_seconds` in the envelope through [`CmdError::RateLimited`].
    fn cmd_error(&self) -> CmdError {
        match self {
            Self::Off => CmdError::MissingApiKey,
            Self::DailyLimit { retry_after } => CmdError::RateLimited {
                retry_after: *retry_after,
            },
            Self::NotReady(msg) => CmdError::Server(msg.clone()),
        }
    }

    /// The human-readable reason and next step, for stderr. The envelope's
    /// message is the stable machine one; these lines say what happened.
    fn note(&self, host: &str) -> String {
        match self {
            Self::Off => format!(
                "taskfast init: no API key was given, and {host} does not create agents \
                 without an account (POST /new answered 404). Nothing was saved.\n  \
                 To create a claimed agent instead: taskfast init --human-api-key <PAT> \
                 (or pass --api-key / TASKFAST_API_KEY)."
            ),
            Self::DailyLimit { retry_after } => format!(
                "taskfast init: {host} refused to create an agent without an account: too \
                 many were created from this address today (POST /new answered 429). \
                 Nothing was saved.\n  Try again in {} seconds.",
                retry_after.as_secs()
            ),
            Self::NotReady(msg) => format!(
                "taskfast init: {host} refused to create an agent without an account: \
                 {msg} (POST /new answered 503). Nothing was saved.\n  Try again later."
            ),
        }
    }
}

/// Map a refused `POST /new` (gh#238) to the error `init` returns, with the
/// reason on stderr for whoever is watching the terminal.
fn new_agent_refused(ctx: &Ctx, outcome: NewAgentOutcome) -> CmdError {
    let refusal = match NewAgentRefusal::from_outcome(&outcome) {
        Some(refusal) => refusal,
        None => return CmdError::Server("POST /new: created an agent on the refusal path".into()),
    };
    eprintln!("{}", refusal.note(ctx.base_url()));
    refusal.cmd_error()
}

fn build_dry_run_mint_envelope(cfg_path: &Path, intent: &MintIntent) -> serde_json::Value {
    json!({
        "agent": {
            "action": intent.kind.dry_run_action(),
            "name": intent.name,
            "description": intent.description,
            "capabilities": intent.capabilities,
        },
        "config_file": {
            "path": cfg_path.display().to_string(),
            "written": false,
            "would_write": true,
        },
        "ready_to_work": false,
    })
}

/// Layered api_key resolution: Ctx (flag / env var / already-merged
/// config value) wins, then the on-disk config, else
/// [`CmdError::MissingApiKey`].
///
/// `Ctx::from_parts` already folds `cfg.api_key` into `ctx.api_key`, so
/// by the time we get here the config lookup is a redundant last resort
/// — kept for the test-driven path that constructs `Ctx` directly
/// without going through `from_parts`.
fn resolve_api_key(ctx: &Ctx, cfg: &Config) -> Result<String, CmdError> {
    if let Some(k) = ctx.api_key.as_deref() {
        if !k.is_empty() {
            return Ok(k.to_string());
        }
    }
    if let Some(k) = cfg.api_key.as_deref() {
        if !k.is_empty() {
            return Ok(k.to_string());
        }
    }
    Err(CmdError::MissingApiKey)
}

/// Build a remediation hint for the common "stale config" failure: the agent
/// api_key stored in the config (or passed via flag/env) is rejected by the
/// server. Lists the key's likely source and the minimum steps to recover.
fn stale_key_hint(msg: &str, cfg_path: &Path, cfg: &Config, ctx: &Ctx, args: &Args) -> String {
    let source = if cfg.api_key.as_deref().is_some_and(|k| !k.is_empty()) {
        format!("config file {}", cfg_path.display())
    } else if ctx.api_key.is_some() {
        "--api-key flag or TASKFAST_API_KEY env".to_string()
    } else {
        "unknown".to_string()
    };
    let had_pat = args.human_api_key.is_some();
    let pat_note = if had_pat {
        "\n  note: --human-api-key was provided but ignored because an existing api_key was found; \
         the PAT mints a new agent only when no api_key is resolvable."
    } else {
        ""
    };
    format!(
        "{msg}\n  hint: agent api_key rejected — likely stale (agent deleted or key rotated).\n  \
         source: {source}\n  fix: delete {path} (and unset TASKFAST_API_KEY if set), then re-run \
         `taskfast --env {env} init --human-api-key <tf_user_...>` to mint a fresh agent.{pat_note}",
        path = cfg_path.display(),
        env = ctx.environment.as_str(),
    )
}

fn assert_active(profile: &taskfast_client::api::types::AgentProfile) -> Result<(), CmdError> {
    match profile.status.as_deref() {
        Some("active") => Ok(()),
        Some(other) => Err(CmdError::Validation {
            code: "agent_not_active".into(),
            message: format!("agent status is {other}; owner must reactivate"),
        }),
        None => Err(CmdError::Server(
            "GET /agents/me returned no status field".into(),
        )),
    }
}

/// Side-effect summary the CLI envelope surfaces to orchestrators.
enum WalletOutcome {
    /// Server already had a wallet on file and the caller didn't override.
    AlreadyConfigured,
    /// BYOW path — caller supplied `--wallet-address`.
    ByoRegistered { address: String },
    /// Generated keypair, saved to keystore, registered with server.
    Generated {
        address: String,
        keystore_path: PathBuf,
    },
    /// Dry-run generate — address is real but keystore wasn't written.
    DryRunGenerated { address: String },
    /// `--skip-wallet` or dry-run BYOW without register.
    Skipped,
}

impl WalletOutcome {
    fn address(&self) -> Option<&str> {
        match self {
            Self::ByoRegistered { address }
            | Self::Generated { address, .. }
            | Self::DryRunGenerated { address } => Some(address),
            Self::AlreadyConfigured | Self::Skipped => None,
        }
    }

    fn keystore_path(&self) -> Option<&Path> {
        match self {
            Self::Generated { keystore_path, .. } => Some(keystore_path),
            _ => None,
        }
    }

    fn tag(&self) -> &'static str {
        match self {
            Self::AlreadyConfigured => "already_configured",
            Self::ByoRegistered { .. } => "byo_registered",
            Self::Generated { .. } => "generated",
            Self::DryRunGenerated { .. } => "dry_run_generated",
            Self::Skipped => "skipped",
        }
    }
}

async fn provision_wallet(
    client: &TaskFastClient,
    args: &Args,
    dry_run: bool,
) -> Result<WalletOutcome, CmdError> {
    if let Some(addr) = args.wallet_address.as_deref() {
        if dry_run {
            return Ok(WalletOutcome::Skipped);
        }
        return match wallet::register_wallet(client, addr)
            .await
            .map_err(CmdError::from)?
        {
            wallet::WalletRegistration::Configured(_) => Ok(WalletOutcome::ByoRegistered {
                address: addr.to_string(),
            }),
            wallet::WalletRegistration::AlreadyConfigured => Err(CmdError::Validation {
                code: "wallet_already_configured".into(),
                message: "agent already has a tempo wallet configured".into(),
            }),
        };
    }
    if !args.generate_wallet {
        return Err(CmdError::Usage(WALLET_FLAG_HINT.into()));
    }

    let signer = wallet::generate_signer();
    let address = format!("0x{}", hex::encode(signer.address().as_slice()));

    if dry_run {
        // Return the address without resolving the password or persisting;
        // caller just needs to see what *would* have been generated.
        return Ok(WalletOutcome::DryRunGenerated { address });
    }

    let password = resolve_wallet_password(args)?;
    let keystore_path = persist_keystore(&signer, args, &password)?;
    match wallet::register_wallet(client, &address).await {
        Ok(wallet::WalletRegistration::Configured(_)) => Ok(WalletOutcome::Generated {
            address,
            keystore_path,
        }),
        Ok(wallet::WalletRegistration::AlreadyConfigured) => {
            let _ = std::fs::remove_file(&keystore_path);
            Err(CmdError::Validation {
                code: "wallet_already_configured".into(),
                message: "agent already has a tempo wallet configured".into(),
            })
        }
        Err(e) => {
            let _ = std::fs::remove_file(&keystore_path);
            Err(CmdError::from(e))
        }
    }
}

fn resolve_wallet_password(args: &Args) -> Result<zeroize::Zeroizing<String>, CmdError> {
    super::wallet_args::resolve_password_with_inline_and_missing_hint(
        args.inline_wallet_password.as_deref(),
        args.wallet_password_file.as_deref(),
        "--generate-wallet requires --wallet-password-file or TASKFAST_WALLET_PASSWORD",
    )
}

fn persist_keystore(
    signer: &PrivateKeySigner,
    args: &Args,
    password: &str,
) -> Result<PathBuf, CmdError> {
    let path = match &args.keystore_path {
        Some(p) => p.clone(),
        None => keystore::default_keyfile_path(signer.address()).map_err(CmdError::from)?,
    };
    keystore::save_signer(signer, &path, password).map_err(CmdError::from)
}

/// Envelope-friendly description of the faucet step. `Skipped` covers the
/// common cases where no request was warranted (mainnet, BYOW, wallet
/// already present, dry-run) — the reason is echoed so the caller can tell
/// *why* we didn't hit the faucet.
enum FaucetOutcome {
    Skipped { reason: &'static str },
    Requested { drops: Vec<FaucetDrop> },
    Failed { error: String },
}

async fn maybe_request_faucet(
    args: &Args,
    network: crate::Network,
    wallet: &WalletOutcome,
    dry_run: bool,
) -> FaucetOutcome {
    if dry_run {
        return FaucetOutcome::Skipped { reason: "dry_run" };
    }
    if !args.fund {
        return FaucetOutcome::Skipped {
            reason: "fund_flag_not_set",
        };
    }
    if !matches!(network, crate::Network::Testnet) {
        return FaucetOutcome::Skipped { reason: "mainnet" };
    }
    let address = match wallet {
        WalletOutcome::Generated { address, .. } => address.clone(),
        WalletOutcome::ByoRegistered { address } => address.clone(),
        _ => {
            return FaucetOutcome::Skipped {
                reason: "no_fresh_wallet",
            };
        }
    };
    let http = reqwest::Client::new();
    match request_testnet_funds(&http, &address).await {
        Ok(drops) => FaucetOutcome::Requested { drops },
        Err(e) => FaucetOutcome::Failed {
            error: e.to_string(),
        },
    }
}

/// Parse the server's hex-encoded wei balance string (`"0x0"` = zero,
/// all-zero hex = zero). Anything unparseable yields `None` = unknown.
fn parse_wei_hex(s: Option<&str>) -> Option<u128> {
    let s = s?.trim();
    let hex = s.strip_prefix("0x").unwrap_or(s);
    if hex.is_empty() {
        return None;
    }
    u128::from_str_radix(hex, 16).ok()
}

/// Build the end-of-init funding warning (gh#142). Fires when the faucet
/// step failed or the wallet balance is zero — a funding gap must not sit
/// quietly next to a successful init, because the next on-chain command
/// (post, escrow sign, settle) would fail. Returns `None` on healthy or
/// unknown states so the warning never becomes noise.
fn build_funding_warning(
    faucet_error: Option<&str>,
    balance_wei: Option<u128>,
    address: &str,
    rpc_url: Option<&str>,
    funding_hint: Option<&str>,
) -> Option<String> {
    let zero_balance = balance_wei.is_some_and(|b| b == 0);
    if faucet_error.is_none() && !zero_balance {
        return None;
    }
    let mut lines = Vec::new();
    match (faucet_error, zero_balance) {
        (Some(err), _) => lines.push(format!(
            "WARNING: testnet faucet funding failed (--fund): {err}"
        )),
        (None, true) => lines.push(format!(
            "WARNING: wallet {address} balance is zero at the end of init"
        )),
        (None, false) => return None,
    }
    lines.push(
        "  the next on-chain command will fail until the wallet is funded. \
         manual fallback:"
            .to_string(),
    );
    if let Some(rpc) = rpc_url {
        lines.push(format!(
            "    cast rpc tempo_fundAddress '[\"{address}\"]' --raw --rpc-url {rpc}"
        ));
    }
    lines.push("  or fund at https://wallet.tempo.xyz".to_string());
    if let Some(hint) = funding_hint {
        lines.push(format!("  server funding hint: {hint}"));
    }
    Some(lines.join("\n"))
}

/// End-of-init funding sanity check (gh#142). Consults the server's
/// balance proxy once and, when the wallet looks unfunded, prints the
/// stderr warning with the manual fallback (Tempo RPC faucet via `cast`,
/// or the web wallet). Best-effort: any balance/RPC lookup failure stays
/// silent — the faucet step's own error (if any) still warns, and the
/// envelope keeps carrying the server's `readiness.checks.funded` gate
/// for machine consumers. Testnet only; mainnet wallets are the owning
/// human's to fund and the envelope already points at wallet.tempo.xyz.
async fn warn_if_unfunded(
    client: &TaskFastClient,
    ctx: &Ctx,
    wallet_address: Option<&str>,
    faucet: &FaucetOutcome,
    readiness: &AgentReadiness,
) {
    if ctx.dry_run {
        return;
    }
    if !matches!(ctx.environment.network(), crate::Network::Testnet) {
        return;
    }
    let Some(address) = wallet_address else {
        return;
    };
    let faucet_error = match faucet {
        FaucetOutcome::Failed { error } => Some(error.as_str()),
        _ => None,
    };
    let balance_wei = match client.inner().get_wallet_balance().await {
        Ok(resp) => parse_wei_hex(resp.into_inner().available_balance.as_deref()),
        Err(e) => {
            tracing::warn!(error = %e, "init: balance check unavailable; skipping zero-balance warning");
            None
        }
    };
    if build_funding_warning(
        faucet_error,
        balance_wei,
        address,
        None,
        readiness.checks.funded.funding_hint.as_deref(),
    )
    .is_none()
    {
        return;
    }
    // Warning will fire — resolve the advertised rpc_url for the cast
    // fallback. Failure just drops the cast line, keeping the web fallback.
    let rpc_url = client
        .fetch_network_config()
        .await
        .ok()
        .and_then(|cfg| cfg.entry(ctx.environment.network().as_str()).ok())
        .map(|entry| entry.rpc_url.clone());
    if let Some(warning) = build_funding_warning(
        faucet_error,
        balance_wei,
        address,
        rpc_url.as_deref(),
        readiness.checks.funded.funding_hint.as_deref(),
    ) {
        eprintln!("{warning}");
    }
}

/// Outcome of the optional webhook step. `Skipped` is the default when
/// `--webhook-url` wasn't passed; the remaining variants mirror the
/// shell-script states (`registered`, `registered + subscribed`,
/// `would_register`).
#[cfg_attr(test, derive(Debug))]
enum WebhookOutcome {
    Skipped,
    DryRun {
        url: String,
        events: Vec<String>,
        secret_file: Option<PathBuf>,
    },
    Registered {
        url: String,
        secret_returned: bool,
        secret_persisted: bool,
        secret_file: Option<PathBuf>,
        subscribed: Option<Vec<String>>,
    },
    Failed {
        error: String,
    },
}

/// Resolve the final event subscription list for `taskfast init
/// --webhook-url`. Precedence: explicit `--webhook-event` flags win
/// outright; absent-events + `--no-default-events` yields an empty list
/// (URL-only registration); absent-events + default path fans out to
/// the 9-event worker default (`webhook::DEFAULT_WORKER_EVENTS`). Shared
/// between the live subscription path and the `--dry-run` envelope so
/// the preview never diverges from what would be pushed.
fn resolve_webhook_events(args: &Args) -> Vec<String> {
    if !args.webhook_events.is_empty() {
        return args.webhook_events.clone();
    }
    if args.no_default_events {
        return Vec::new();
    }
    crate::cmd::webhook::DEFAULT_WORKER_EVENTS
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

async fn maybe_configure_webhook(
    client: &TaskFastClient,
    args: &Args,
    dry_run: bool,
) -> WebhookOutcome {
    let Some(url) = args.webhook_url.as_deref().filter(|s| !s.trim().is_empty()) else {
        return WebhookOutcome::Skipped;
    };
    let events = resolve_webhook_events(args);
    if dry_run {
        return WebhookOutcome::DryRun {
            url: url.to_string(),
            events,
            secret_file: args.webhook_secret_file.clone(),
        };
    }
    match crate::cmd::webhook::provision_webhook(
        client,
        url,
        args.webhook_secret_file.as_deref(),
        &events,
    )
    .await
    {
        Ok(outcome) => WebhookOutcome::Registered {
            url: outcome.url,
            secret_returned: outcome.secret_returned,
            secret_persisted: outcome.secret_persisted,
            secret_file: outcome.secret_file,
            subscribed: outcome.subscribed,
        },
        Err(e) => WebhookOutcome::Failed {
            error: e.to_string(),
        },
    }
}

fn build_envelope_data(
    cfg_path: &Path,
    config_written: bool,
    wallet: &WalletOutcome,
    agent: &AgentOutcome,
    readiness: &AgentReadiness,
    faucet: &FaucetOutcome,
    webhook: &WebhookOutcome,
    dry_run: bool,
) -> serde_json::Value {
    let mut wallet_obj = json!({
        "status": wallet.tag(),
    });
    if let Some(addr) = wallet.address() {
        wallet_obj["address"] = json!(addr);
    }
    if let Some(path) = wallet.keystore_path() {
        wallet_obj["keystore_path"] = json!(path.display().to_string());
    }

    let mut cfg_obj = json!({
        "path": cfg_path.display().to_string(),
        "written": config_written,
    });
    if dry_run && !config_written {
        cfg_obj["would_write"] = json!(true);
    }

    let faucet_obj = match faucet {
        FaucetOutcome::Skipped { reason } => {
            let mut o = json!({
                "status": "skipped",
                "reason": reason,
            });
            // On mainnet, point the caller at the human funding path so
            // they don't assume the CLI "just handles it" like it does on
            // testnet.
            if *reason == "mainnet" {
                o["funding_hint"] = json!({
                    "url": "https://wallet.tempo.xyz",
                    "message": "Production wallets must be funded manually by the owning human at wallet.tempo.xyz before posting or settling.",
                });
            }
            o
        }
        FaucetOutcome::Requested { drops } => json!({
            "status": "requested",
            "drops": drops.iter().map(|d| json!({
                "token": d.token,
                "tx_hash": d.tx_hash,
            })).collect::<Vec<_>>(),
        }),
        FaucetOutcome::Failed { error } => json!({
            "status": "failed",
            "error": error,
        }),
    };

    let webhook_obj = match webhook {
        WebhookOutcome::Skipped => json!({ "status": "skipped" }),
        WebhookOutcome::DryRun {
            url,
            events,
            secret_file,
        } => {
            let mut o = json!({
                "status": "would_register",
                "url": url,
                "events": events,
            });
            if let Some(p) = secret_file {
                o["secret_file"] = json!(p.display().to_string());
            }
            o
        }
        WebhookOutcome::Registered {
            url,
            secret_returned,
            secret_persisted,
            secret_file,
            subscribed,
        } => {
            let mut o = json!({
                "status": "registered",
                "url": url,
                "secret_returned": secret_returned,
                "secret_persisted": secret_persisted,
            });
            if let Some(p) = secret_file {
                o["secret_file"] = json!(p.display().to_string());
            }
            if let Some(subs) = subscribed {
                o["subscribed"] = json!(subs);
            }
            o
        }
        WebhookOutcome::Failed { error } => json!({
            "status": "failed",
            "error": error,
        }),
    };

    let mut out = json!({
        "wallet": wallet_obj,
        "config_file": cfg_obj,
        "faucet": faucet_obj,
        "webhook": webhook_obj,
        "readiness": readiness,
        "ready_to_work": readiness.ready_to_work,
    });
    if let AgentOutcome::Minted(MintedAgent::Live {
        id,
        name,
        unclaimed,
        ..
    }) = agent
    {
        let mut a = json!({
            // Orchestrators branch on this: `minted` is owned from the
            // start, `minted_unclaimed` needs a person to claim it.
            "action": if unclaimed.is_some() { "minted_unclaimed" } else { "minted" },
        });
        if let Some(id) = id {
            a["id"] = json!(id);
        }
        if let Some(name) = name {
            a["name"] = json!(name);
        }
        if let Some(u) = unclaimed {
            if let Some(url) = &u.claim_url {
                a["claim_url"] = json!(url);
            }
            if let Some(expires_at) = u.expires_at {
                a["expires_at"] = json!(expires_at);
            }
            if let Some(url) = &u.terms_url {
                a["terms_url"] = json!(url);
            }
            if let Some(version) = &u.terms_version {
                a["terms_version"] = json!(version);
            }
            if let Some(accepted) = u.terms_accepted {
                a["terms_accepted"] = json!(accepted);
            }
        }
        out["agent"] = a;
    }
    out
}

/// The stderr note for an agent that waits to be claimed (gh#238): where
/// to claim it, until when, and what the request accepted. The Terms
/// sentence is the server's, verbatim — legal wording is the operator's
/// to approve, not this crate's to paraphrase — and is printed only when
/// the server itself says the Terms were accepted. `None` when the run did
/// not just create such an agent, or the server sent no link to announce.
fn unclaimed_announcement(agent: &AgentOutcome) -> Option<String> {
    let AgentOutcome::Minted(MintedAgent::Live {
        name,
        unclaimed: Some(unclaimed),
        ..
    }) = agent
    else {
        return None;
    };
    let claim_url = unclaimed.claim_url.as_deref()?;
    let mut lines = Vec::new();
    lines.push(match name {
        Some(name) => format!(
            "Created agent \"{name}\" without an account. Nobody owns it until a person \
             claims it, and it cannot accept a bid before that."
        ),
        None => "Created an agent without an account. Nobody owns it until a person claims \
             it, and it cannot accept a bid before that."
            .to_string(),
    });
    lines.push(format!(
        "  Claim link (share it with your owner): {claim_url}"
    ));
    if let Some(expires_at) = unclaimed.expires_at {
        lines.push(format!(
            "  Its key stops working at {expires_at} unless it is claimed first."
        ));
    }
    // Only the server may say the Terms were accepted. The line is printed
    // when it said so (`terms_accepted`): in its words when it sent a
    // notice, else as a plain pointer to the document — never a sentence
    // this crate composed from the URL alone (gh#238 review).
    if unclaimed.terms_accepted == Some(true) {
        let terms = match (
            unclaimed.terms_notice.as_deref(),
            unclaimed.terms_url.as_deref(),
        ) {
            (Some(notice), _) => notice.to_string(),
            (None, Some(url)) => format!("Terms of Service for agents: {url}."),
            (None, None) => String::new(),
        };
        if !terms.is_empty() {
            match unclaimed.terms_version.as_deref() {
                Some(version) => lines.push(format!("  {terms} Terms version {version}.")),
                None => lines.push(format!("  {terms}")),
            }
        }
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Environment;

    fn base_args() -> Args {
        Args {
            wallet_address: None,
            generate_wallet: false,
            wallet_password_file: None,
            keystore_path: None,
            skip_wallet: false,
            fund: false,
            human_api_key: None,
            agent_name: "taskfast-agent".into(),
            agent_description: "Headless agent registered via taskfast init".into(),
            agent_capabilities: Vec::new(),
            webhook_url: None,
            webhook_secret_file: None,
            webhook_events: Vec::new(),
            no_default_events: false,
            no_interactive: true,
            inline_wallet_password: None,
        }
    }

    fn ctx_with_key(key: Option<&str>) -> Ctx {
        Ctx {
            api_key: key.map(String::from),
            environment: Environment::Local,
            api_base: None,
            config_path: PathBuf::from("/dev/null"),
            dry_run: false,
            quiet: true,
            ..Default::default()
        }
    }

    fn cfg_with_key(key: Option<&str>) -> Config {
        Config {
            api_key: key.map(str::to_string),
            ..Config::default()
        }
    }

    #[test]
    fn resolve_api_key_prefers_ctx_over_config() {
        let ctx = ctx_with_key(Some("from-flag"));
        let cfg = cfg_with_key(Some("from-file"));
        assert_eq!(resolve_api_key(&ctx, &cfg).unwrap(), "from-flag");
    }

    #[test]
    fn resolve_api_key_falls_back_to_config() {
        let ctx = ctx_with_key(None);
        let cfg = cfg_with_key(Some("from-file"));
        assert_eq!(resolve_api_key(&ctx, &cfg).unwrap(), "from-file");
    }

    #[test]
    fn resolve_api_key_empty_string_is_treated_as_absent() {
        let ctx = ctx_with_key(Some(""));
        let cfg = cfg_with_key(Some(""));
        match resolve_api_key(&ctx, &cfg) {
            Err(CmdError::MissingApiKey) => {}
            other => panic!("expected MissingApiKey, got {other:?}"),
        }
    }

    #[test]
    fn provision_without_wallet_flag_errors_as_usage() {
        // We can't easily drive provision_wallet without a client, but we
        // can prove the flag-gate logic: with no flags set, the error is
        // Usage, not MissingApiKey.
        let args = base_args();
        assert!(args.wallet_address.is_none() && !args.generate_wallet);
        // The branch that returns Usage lives in provision_wallet — a
        // dedicated integration test drives the end-to-end path.
    }

    #[test]
    fn wallet_outcome_tag_is_stable() {
        // Pinning the tag strings is intentional: orchestrators branch on
        // `data.wallet.status` so changes here are breaking.
        assert_eq!(WalletOutcome::AlreadyConfigured.tag(), "already_configured");
        assert_eq!(
            WalletOutcome::ByoRegistered {
                address: "0x00".into()
            }
            .tag(),
            "byo_registered"
        );
        assert_eq!(
            WalletOutcome::Generated {
                address: "0x00".into(),
                keystore_path: PathBuf::from("/tmp/x")
            }
            .tag(),
            "generated"
        );
        assert_eq!(
            WalletOutcome::DryRunGenerated {
                address: "0x00".into()
            }
            .tag(),
            "dry_run_generated"
        );
        assert_eq!(WalletOutcome::Skipped.tag(), "skipped");
    }

    #[test]
    fn resolve_webhook_events_uses_defaults_when_events_empty() {
        // URL set, no explicit events, no opt-out → auto-subscribe to
        // the 9-event worker default. Guards PLAN #11: raw `init
        // --webhook-url X` must not silently register a zero-event
        // endpoint.
        let args = Args {
            webhook_url: Some("https://h.example/x".into()),
            ..base_args()
        };
        let events = resolve_webhook_events(&args);
        assert_eq!(events, crate::cmd::webhook::DEFAULT_WORKER_EVENTS);
    }

    #[test]
    fn resolve_webhook_events_respects_no_default_events_opt_out() {
        // Explicit opt-out preserves the legacy URL-only behavior.
        let args = Args {
            webhook_url: Some("https://h.example/x".into()),
            no_default_events: true,
            ..base_args()
        };
        let events = resolve_webhook_events(&args);
        assert!(events.is_empty(), "expected no events, got {events:?}");
    }

    #[test]
    fn resolve_webhook_events_explicit_events_override_defaults() {
        // Any `--webhook-event` wins: the defaults are NOT mixed in.
        let args = Args {
            webhook_url: Some("https://h.example/x".into()),
            webhook_events: vec!["task_assigned".into()],
            ..base_args()
        };
        assert_eq!(resolve_webhook_events(&args), vec!["task_assigned"]);
    }

    #[test]
    fn resolve_webhook_events_explicit_events_win_over_no_default_events() {
        // Safety: --no-default-events is an opt-out flag for the
        // auto-population path; explicit events should still flow.
        let args = Args {
            webhook_url: Some("https://h.example/x".into()),
            webhook_events: vec!["bid_accepted".into()],
            no_default_events: true,
            ..base_args()
        };
        assert_eq!(resolve_webhook_events(&args), vec!["bid_accepted"]);
    }

    // ─── end-of-init funding warning (gh#142) ─────────────────────────────

    #[test]
    fn funding_warning_faucet_failure_lists_manual_fallback() {
        let w = build_funding_warning(
            Some("faucet returned status 405: method not allowed"),
            Some(0),
            "0xabc0000000000000000000000000000000000000",
            Some("https://rpc.testnet.example"),
            None,
        )
        .expect("faucet failure must warn");
        assert!(w.contains("faucet returned status 405"), "got: {w}");
        assert!(w.contains("0xabc0000000000000000000000000000000000000"));
        assert!(w.contains(
            "cast rpc tempo_fundAddress \
             '[\"0xabc0000000000000000000000000000000000000\"]' \
             --raw --rpc-url https://rpc.testnet.example"
        ));
        assert!(w.contains("https://wallet.tempo.xyz"));
    }

    #[test]
    fn funding_warning_zero_balance_without_faucet_failure_still_warns() {
        // The issue's second trigger: `--fund` succeeded or was never
        // passed, but the wallet balance is zero at the end of init —
        // the next on-chain command would fail.
        let w = build_funding_warning(
            None,
            Some(0),
            "0xabc0000000000000000000000000000000000000",
            Some("https://rpc.testnet.example"),
            None,
        )
        .expect("zero balance must warn");
        assert!(w.contains("zero"), "got: {w}");
        assert!(w.contains("https://wallet.tempo.xyz"));
    }

    #[test]
    fn funding_warning_silent_when_funded() {
        assert!(build_funding_warning(
            None,
            Some(42),
            "0xabc0000000000000000000000000000000000000",
            Some("https://rpc.testnet.example"),
            None,
        )
        .is_none());
    }

    #[test]
    fn funding_warning_silent_when_balance_unknown_and_faucet_did_not_fail() {
        // Balance endpoint down + faucet didn't fail → no signal, no noise.
        assert!(build_funding_warning(
            None,
            None,
            "0xabc0000000000000000000000000000000000000",
            Some("https://rpc.testnet.example"),
            None,
        )
        .is_none());
    }

    #[test]
    fn funding_warning_without_rpc_url_omits_cast_line_keeps_web_fallback() {
        let w = build_funding_warning(
            Some("faucet http request failed"),
            Some(0),
            "0xabc0000000000000000000000000000000000000",
            None,
            None,
        )
        .expect("must still warn");
        assert!(!w.contains("cast rpc"), "got: {w}");
        assert!(w.contains("https://wallet.tempo.xyz"));
    }

    #[test]
    fn funding_warning_surfaces_server_funding_hint() {
        let w = build_funding_warning(
            None,
            Some(0),
            "0xabc0000000000000000000000000000000000000",
            None,
            Some("https://wallet.tempo.xyz/topup"),
        )
        .expect("must warn");
        assert!(w.contains("https://wallet.tempo.xyz/topup"), "got: {w}");
    }

    #[test]
    fn parse_wei_hex_handles_0x_zero_and_plain_hex_forms() {
        assert_eq!(parse_wei_hex(Some("0x0")), Some(0));
        assert_eq!(
            parse_wei_hex(Some(
                "0x000000000000000000000000000000000000000000000000000000000000000a"
            )),
            Some(10)
        );
        assert_eq!(parse_wei_hex(None), None);
        assert_eq!(parse_wei_hex(Some("0x")), None);
    }

    #[tokio::test]
    async fn maybe_configure_webhook_dry_run_surfaces_default_events() {
        // Dry-run envelope must preview what the live path would push —
        // otherwise `init --webhook-url X --dry-run` silently differs
        // from the real call.
        let client = TaskFastClient::from_api_key("http://127.0.0.1:1/", "k").unwrap();
        let args = Args {
            webhook_url: Some("https://h.example/x".into()),
            ..base_args()
        };
        let outcome = maybe_configure_webhook(&client, &args, true).await;
        match outcome {
            WebhookOutcome::DryRun { events, url, .. } => {
                assert_eq!(url, "https://h.example/x");
                assert_eq!(events, crate::cmd::webhook::DEFAULT_WORKER_EVENTS);
            }
            other => panic!("expected DryRun, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn maybe_configure_webhook_dry_run_respects_no_default_events() {
        let client = TaskFastClient::from_api_key("http://127.0.0.1:1/", "k").unwrap();
        let args = Args {
            webhook_url: Some("https://h.example/x".into()),
            no_default_events: true,
            ..base_args()
        };
        let outcome = maybe_configure_webhook(&client, &args, true).await;
        match outcome {
            WebhookOutcome::DryRun { events, .. } => {
                assert!(events.is_empty(), "expected no events, got {events:?}");
            }
            other => panic!("expected DryRun, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn maybe_configure_webhook_no_url_is_skipped() {
        // Regression guard: the auto-subscribe branch must not fire
        // when the URL is absent — init stays single-purpose.
        let client = TaskFastClient::from_api_key("http://127.0.0.1:1/", "k").unwrap();
        let outcome = maybe_configure_webhook(&client, &base_args(), true).await;
        assert!(matches!(outcome, WebhookOutcome::Skipped));
    }

    // ─── no key → POST /new (gh#238) ────────────────────────────────────────

    fn created_outcome() -> NewAgentOutcome {
        NewAgentOutcome::Created(Box::default())
    }

    #[test]
    fn refusal_comes_from_the_typed_outcome_not_from_error_display() {
        // The client decides the documented refusals on the raw response;
        // here they only map to the CLI's note + error. Nothing parses an
        // error string (gh#238 review).
        use std::time::Duration;
        assert_eq!(
            NewAgentRefusal::from_outcome(&NewAgentOutcome::RouteOff),
            Some(NewAgentRefusal::Off)
        );
        assert_eq!(
            NewAgentRefusal::from_outcome(&NewAgentOutcome::RateLimited {
                retry_after: Duration::from_hours(24),
            }),
            Some(NewAgentRefusal::DailyLimit {
                retry_after: Duration::from_hours(24),
            })
        );
        assert_eq!(
            NewAgentRefusal::from_outcome(&NewAgentOutcome::NotReady {
                message: "The Terms of Service are not published yet; try again later.".into(),
            }),
            Some(NewAgentRefusal::NotReady(
                "The Terms of Service are not published yet; try again later.".into()
            ))
        );
        // A created agent is not a refusal; the caller matches it first.
        assert_eq!(NewAgentRefusal::from_outcome(&created_outcome()), None);
    }

    #[test]
    fn refusal_off_keeps_the_keyless_error_and_exit_code() {
        // The error a keyless run always had, so `missing_api_key` handling
        // in existing orchestrators keeps working on deployments without
        // the route.
        assert!(matches!(
            NewAgentRefusal::Off.cmd_error(),
            CmdError::MissingApiKey
        ));
    }

    #[test]
    fn refusal_daily_limit_keeps_retry_after_for_the_envelope() {
        use std::time::Duration;
        match (NewAgentRefusal::DailyLimit {
            retry_after: Duration::from_hours(24),
        })
        .cmd_error()
        {
            CmdError::RateLimited { retry_after } => {
                assert_eq!(retry_after, Duration::from_hours(24))
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn refusal_not_ready_carries_the_server_wording() {
        match NewAgentRefusal::NotReady(
            "The Terms of Service are not published yet; try again later.".into(),
        )
        .cmd_error()
        {
            CmdError::Server(msg) => assert_eq!(
                msg,
                "The Terms of Service are not published yet; try again later."
            ),
            other => panic!("expected Server, got {other:?}"),
        }
    }

    #[test]
    fn refusal_notes_say_what_happened_and_what_next() {
        use std::time::Duration;
        let off = NewAgentRefusal::Off.note("https://staging.api.taskfast.app");
        assert!(off.contains("https://staging.api.taskfast.app"), "{off}");
        assert!(off.contains("404"), "{off}");
        assert!(off.contains("--human-api-key"), "{off}");
        assert!(off.contains("Nothing was saved"), "{off}");

        let limited = NewAgentRefusal::DailyLimit {
            retry_after: Duration::from_hours(24),
        }
        .note("https://h");
        assert!(limited.contains("429"), "{limited}");
        assert!(limited.contains("86400 seconds"), "{limited}");
        assert!(limited.contains("Nothing was saved"), "{limited}");

        let not_ready =
            NewAgentRefusal::NotReady("Terms are not published yet.".into()).note("https://h");
        assert!(
            not_ready.contains("Terms are not published yet."),
            "{not_ready}"
        );
        assert!(not_ready.contains("503"), "{not_ready}");
        assert!(not_ready.contains("Nothing was saved"), "{not_ready}");
    }

    #[test]
    fn dry_run_action_distinguishes_the_two_mints() {
        // Orchestrators branch on `data.agent.action`; the PAT preview is
        // unchanged and the new one gets its own tag.
        assert_eq!(MintKind::FromPat.dry_run_action(), "would_mint");
        assert_eq!(MintKind::Unclaimed.dry_run_action(), "would_mint_unclaimed");
    }

    #[test]
    fn wallet_preflight_demands_a_signal_before_the_mint() {
        // gh#238 review, P1: the usage error `provision_wallet` would raise
        // after a 201 is raised before the request instead, with the very
        // same wording.
        let no_signal = base_args();
        match preflight_wallet_args(&no_signal) {
            Err(CmdError::Usage(msg)) => assert_eq!(msg, WALLET_FLAG_HINT),
            other => panic!("expected Usage, got {other:?}"),
        }
        // Each way of satisfying the wallet step passes. (The password
        // failure depends on `TASKFAST_WALLET_PASSWORD`, which wins over
        // the file in `wallet_args` and is set process-wide by that
        // module's tests behind a private lock — so that case is pinned by
        // the integration test, which can skip when the variable is set.)
        let mut skip = base_args();
        skip.skip_wallet = true;
        assert!(preflight_wallet_args(&skip).is_ok());
        let mut byow = base_args();
        byow.wallet_address = Some("0xabc".into());
        assert!(preflight_wallet_args(&byow).is_ok());
        let mut inline = base_args();
        inline.generate_wallet = true;
        inline.inline_wallet_password = Some("pw".into());
        assert!(preflight_wallet_args(&inline).is_ok());
    }

    #[tokio::test]
    async fn blank_name_or_description_is_rejected_before_the_mint() {
        // The spec's length limits let whitespace through; the server trims
        // and refuses — after counting the request. Mirrored here.
        let mut args = base_args();
        args.agent_name = "   ".into();
        let err = create_unclaimed(&ctx_with_key(None), &args)
            .await
            .expect_err("blank name");
        assert!(
            matches!(err, CmdError::Usage(ref m) if m.contains("--agent-name")),
            "{err:?}"
        );
        let mut args = base_args();
        args.agent_description = "\t\n".into();
        let err = create_unclaimed(&ctx_with_key(None), &args)
            .await
            .expect_err("blank description");
        assert!(
            matches!(err, CmdError::Usage(ref m) if m.contains("--agent-description")),
            "{err:?}"
        );
    }

    #[test]
    fn capability_note_names_the_flags_and_the_way_out() {
        let note = capabilities_ignored_note(&["research".into(), "sql".into()]);
        assert!(
            note.starts_with("WARNING: --agent-capability is ignored"),
            "{note}"
        );
        assert!(
            note.contains("--agent-capability research, --agent-capability sql"),
            "{note}"
        );
        assert!(note.contains("claims"), "{note}");
        assert!(note.contains("taskfast agent update-me"), "{note}");
    }

    fn live_unclaimed(name: Option<&str>) -> AgentOutcome {
        AgentOutcome::Minted(MintedAgent::Live {
            api_key: "am_live_x".into(),
            id: Some("22222222-2222-2222-2222-222222222222".into()),
            name: name.map(str::to_string),
            unclaimed: Some(Unclaimed {
                claim_url: Some("https://staging.taskfast.app/new/claim?claim_token=tok".into()),
                expires_at: Some("2026-11-26T17:48:51Z".parse().unwrap()),
                terms_url: Some("https://staging.taskfast.app/terms/agents".into()),
                terms_version: Some("2026-11-15".into()),
                terms_notice: Some(
                    "By sending this request you accepted the TaskFast Terms of Service at \
                     https://staging.taskfast.app/terms/agents."
                        .into(),
                ),
                terms_accepted: Some(true),
            }),
        })
    }

    #[test]
    fn announcement_names_the_link_the_deadline_and_the_server_terms_sentence() {
        let note = unclaimed_announcement(&live_unclaimed(Some("taskfast-agent")))
            .expect("an unclaimed agent is announced");
        assert!(note.contains("\"taskfast-agent\""), "{note}");
        assert!(
            note.contains("https://staging.taskfast.app/new/claim?claim_token=tok"),
            "{note}"
        );
        assert!(note.contains("2026-11-26T17:48:51Z"), "{note}");
        // Legal wording is the operator's: the server's sentence is
        // reproduced as is, with its version.
        assert!(
            note.contains(
                "By sending this request you accepted the TaskFast Terms of Service at \
                 https://staging.taskfast.app/terms/agents. Terms version 2026-11-15."
            ),
            "{note}"
        );
    }

    #[test]
    fn announcement_without_a_notice_points_at_the_terms_but_only_if_accepted() {
        // No notice: a plain pointer, never a sentence of this crate's own
        // claiming acceptance — and nothing at all unless the server said
        // the Terms were accepted (gh#238 review).
        let AgentOutcome::Minted(MintedAgent::Live { unclaimed, .. }) = live_unclaimed(None) else {
            unreachable!()
        };
        let mut unclaimed = unclaimed.unwrap();
        unclaimed.terms_notice = None;
        unclaimed.expires_at = None;
        let note = unclaimed_announcement(&AgentOutcome::Minted(MintedAgent::Live {
            api_key: "am_live_x".into(),
            id: None,
            name: None,
            unclaimed: Some(unclaimed.clone()),
        }))
        .expect("still announced");
        assert!(
            note.contains("Created an agent without an account"),
            "{note}"
        );
        assert!(!note.contains("stops working"), "{note}");
        assert!(
            note.contains(
                "Terms of Service for agents: https://staging.taskfast.app/terms/agents. \
                 Terms version 2026-11-15."
            ),
            "{note}"
        );
        assert!(!note.contains("accepted"), "{note}");

        unclaimed.terms_accepted = None;
        let note = unclaimed_announcement(&AgentOutcome::Minted(MintedAgent::Live {
            api_key: "am_live_x".into(),
            id: None,
            name: None,
            unclaimed: Some(unclaimed),
        }))
        .expect("still announced");
        assert!(!note.contains("Terms"), "{note}");
    }

    #[test]
    fn announcement_is_silent_for_other_outcomes() {
        assert!(unclaimed_announcement(&AgentOutcome::PreExisting).is_none());
        assert!(
            unclaimed_announcement(&AgentOutcome::Minted(MintedAgent::Live {
                api_key: "am_live_x".into(),
                id: None,
                name: None,
                unclaimed: None,
            }))
            .is_none()
        );
        // No link, nothing to announce.
        let AgentOutcome::Minted(MintedAgent::Live { unclaimed, .. }) = live_unclaimed(None) else {
            unreachable!()
        };
        let mut unclaimed = unclaimed.unwrap();
        unclaimed.claim_url = None;
        assert!(
            unclaimed_announcement(&AgentOutcome::Minted(MintedAgent::Live {
                api_key: "am_live_x".into(),
                id: None,
                name: None,
                unclaimed: Some(unclaimed),
            }))
            .is_none()
        );
    }

    #[test]
    fn envelope_agent_object_carries_the_claim_half_only_for_post_new() {
        let readiness: AgentReadiness = serde_json::from_value(json!({
            "ready_to_work": true,
            "checks": {
                "api_key": {"status": "complete"},
                "wallet": {"status": "complete"},
                "funded": {"status": "complete"},
                "webhook": {"status": "not_configured", "required": false},
            },
            "settlement_domain": {
                "chain_id": 42431,
                "verifying_contract": "0x0000000000000000000000000000000000000000",
            },
        }))
        .unwrap();
        let data = build_envelope_data(
            Path::new("/dev/null"),
            true,
            &WalletOutcome::Skipped,
            &live_unclaimed(Some("taskfast-agent")),
            &readiness,
            &FaucetOutcome::Skipped { reason: "dry_run" },
            &WebhookOutcome::Skipped,
            false,
        );
        assert_eq!(data["agent"]["action"], "minted_unclaimed");
        assert_eq!(
            data["agent"]["claim_url"],
            "https://staging.taskfast.app/new/claim?claim_token=tok"
        );
        assert_eq!(data["agent"]["expires_at"], "2026-11-26T17:48:51Z");
        assert_eq!(data["agent"]["terms_version"], "2026-11-15");
        assert_eq!(data["agent"]["terms_accepted"], true);

        // The PAT mint keeps its old shape: action `minted`, no claim fields.
        let pat = AgentOutcome::Minted(MintedAgent::Live {
            api_key: "am_live_x".into(),
            id: Some("11111111-1111-1111-1111-111111111111".into()),
            name: Some("taskfast-agent".into()),
            unclaimed: None,
        });
        let data = build_envelope_data(
            Path::new("/dev/null"),
            true,
            &WalletOutcome::Skipped,
            &pat,
            &readiness,
            &FaucetOutcome::Skipped { reason: "dry_run" },
            &WebhookOutcome::Skipped,
            false,
        );
        assert_eq!(data["agent"]["action"], "minted");
        assert!(data["agent"].get("claim_url").is_none());
    }
}
