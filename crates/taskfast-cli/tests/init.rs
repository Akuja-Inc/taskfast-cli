// SPDX-License-Identifier: MIT
//! End-to-end tests for `taskfast init`.
//!
//! Covers the full command pipeline (api-key resolution, validate,
//! readiness, wallet provisioning, config persistence, final readiness)
//! against a wiremock server.

use std::fs;
use std::path::PathBuf;

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::init::{run, Args};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::config::Config;
use taskfast_cli::{Envelope, Environment};

const BYOW_ADDRESS: &str = "0xdEaDbEeF00000000000000000000000000000001";

fn ctx_for(server: &MockServer, key: Option<&str>, config_path: PathBuf, dry_run: bool) -> Ctx {
    Ctx {
        api_key: key.map(String::from),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        config_path,
        dry_run,
        quiet: true,
        ..Default::default()
    }
}

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

/// `base_args()` plus the one thing a keyless run now has to carry before
/// `POST /new` is tried (gh#238 review, P1): a wallet signal. A fresh
/// agent never has a wallet, so without one the preflight stops the run
/// before any request.
fn keyless_args() -> Args {
    Args {
        skip_wallet: true,
        ..base_args()
    }
}

fn config_path_in(dir: &std::path::Path) -> PathBuf {
    dir.join(".taskfast").join("config.json")
}

fn envelope_value(env: &Envelope) -> serde_json::Value {
    serde_json::to_value(env).expect("serialize envelope")
}

async fn mount_profile_active(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "00000000-0000-0000-0000-000000000042",
            "name": "alice",
            "status": "active",
            "capabilities": ["coding"],
        })))
        .mount(server)
        .await;
}

async fn mount_readiness(server: &MockServer, wallet_status: &str, ready_to_work: bool) {
    let funded_status = if wallet_status == "complete" {
        "complete"
    } else {
        "no_wallet"
    };
    Mock::given(method("GET"))
        .and(path("/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ready_to_work": ready_to_work,
            "checks": {
                "api_key": {"status": "complete"},
                "wallet": {"status": wallet_status},
                "funded": {"status": funded_status},
                "webhook": {"status": "not_configured", "required": false},
            },
            "settlement_domain": {
                "chain_id": 42431,
                "verifying_contract": "0x0000000000000000000000000000000000000000",
            },
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn byow_happy_path_registers_wallet_and_writes_env_file() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;

    Mock::given(method("POST"))
        .and(path("/agents/me/wallet"))
        .and(body_partial_json(json!({
            "tempo_wallet_address": BYOW_ADDRESS,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tempo_wallet_address": BYOW_ADDRESS,
            "payout_method": "tempo_wallet",
            "payment_method": "tempo",
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let mut args = base_args();
    args.wallet_address = Some(BYOW_ADDRESS.to_string());

    let envelope = run(
        &ctx_for(&server, Some("test-key"), cfg_path.clone(), false),
        args,
    )
    .await
    .expect("init should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["wallet"]["status"], "byo_registered");
    assert_eq!(v["data"]["wallet"]["address"], BYOW_ADDRESS);
    assert_eq!(v["data"]["config_file"]["written"], true);

    // Config exists and carries the registered address + api key.
    // `api_base` and `network` are no longer persisted — both derive from
    // `environment` at runtime.
    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(loaded.api_key.as_deref(), Some("test-key"));
    assert_eq!(loaded.wallet_address.as_deref(), Some(BYOW_ADDRESS));
    assert_eq!(loaded.environment, Some(Environment::Local));
}

#[tokio::test]
async fn byow_409_already_configured_returns_validation_error() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;

    Mock::given(method("POST"))
        .and(path("/agents/me/wallet"))
        .and(body_partial_json(json!({
            "tempo_wallet_address": BYOW_ADDRESS,
        })))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "wallet_already_configured",
            "message": "agent already has a tempo wallet configured",
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let mut args = base_args();
    args.wallet_address = Some(BYOW_ADDRESS.to_string());

    let err = run(
        &ctx_for(&server, Some("test-key"), cfg_path.clone(), false),
        args,
    )
    .await
    .expect_err("409 wallet_already_configured with explicit address must error");

    match err {
        CmdError::Validation { code, .. } => {
            assert_eq!(code, "wallet_already_configured");
        }
        other => panic!("expected Validation error, got {other:?}"),
    }
}

#[tokio::test]
async fn generate_wallet_409_already_configured_returns_validation_error_and_cleans_up() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;

    Mock::given(method("POST"))
        .and(path("/agents/me/wallet"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "error": "wallet_already_configured",
            "message": "agent already has a tempo wallet configured",
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let pw_path = tmp.path().join("pw");
    fs::write(&pw_path, "s3kret\n").unwrap();
    let keystore_path = tmp.path().join("wallet.json");

    let mut args = base_args();
    args.generate_wallet = true;
    args.wallet_password_file = Some(pw_path);
    args.keystore_path = Some(keystore_path.clone());

    let err = run(
        &ctx_for(&server, Some("test-key"), cfg_path.clone(), false),
        args,
    )
    .await
    .expect_err("409 wallet_already_configured with generate_wallet must error");

    match err {
        CmdError::Validation { code, .. } => {
            assert_eq!(code, "wallet_already_configured");
        }
        other => panic!("expected Validation error, got {other:?}"),
    }
    assert!(
        !keystore_path.exists(),
        "keystore must be cleaned up on failed registration"
    );
}

#[tokio::test]
async fn init_provisions_webhook_and_subscriptions_end_to_end() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", false).await;

    Mock::given(method("PUT"))
        .and(path("/agents/me/webhooks"))
        .and(body_partial_json(json!({
            "url": "https://example.com/init-hook",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "created_at": "2026-04-13T00:00:00Z",
            "updated_at": "2026-04-13T00:00:00Z",
            "url": "https://example.com/init-hook",
            "events": ["task_assigned"],
            "secret": "whsec_first_time_only",
        })))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/agents/me/webhooks/subscriptions"))
        .and(body_partial_json(json!({
            "subscribed_event_types": ["task_assigned"],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "available_event_types": ["task_assigned", "bid_accepted"],
            "subscribed_event_types": ["task_assigned"],
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let secret_file = tmp.path().join("hook.secret");
    let mut args = base_args();
    args.skip_wallet = true;
    args.webhook_url = Some("https://example.com/init-hook".into());
    args.webhook_secret_file = Some(secret_file.clone());
    args.webhook_events = vec!["task_assigned".into()];

    let envelope = run(
        &ctx_for(&server, Some("test-key"), config_path_in(tmp.path()), false),
        args,
    )
    .await
    .expect("init with webhook provisioning must succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["webhook"]["status"], "registered");
    assert_eq!(v["data"]["webhook"]["secret_returned"], true);
    assert_eq!(v["data"]["webhook"]["secret_persisted"], true);
    assert_eq!(v["data"]["webhook"]["subscribed"], json!(["task_assigned"]));

    let secret_contents = std::fs::read_to_string(&secret_file).unwrap();
    assert_eq!(secret_contents, "whsec_first_time_only");
}

#[tokio::test]
async fn skips_wallet_when_server_already_has_one() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;

    let tmp = TempDir::new().unwrap();
    let args = base_args();

    let envelope = run(
        &ctx_for(&server, Some("test-key"), config_path_in(tmp.path()), false),
        args,
    )
    .await
    .expect("init should succeed without wallet flags");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["wallet"]["status"], "already_configured");
    assert_eq!(v["data"]["ready_to_work"], true);
}

#[tokio::test]
async fn skip_wallet_flag_bypasses_provisioning_even_when_missing() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;

    let tmp = TempDir::new().unwrap();
    let mut args = base_args();
    args.skip_wallet = true;

    let envelope = run(
        &ctx_for(&server, Some("test-key"), config_path_in(tmp.path()), false),
        args,
    )
    .await
    .expect("--skip-wallet should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["wallet"]["status"], "skipped");
    // Final readiness reflects server state, not caller intent.
    assert_eq!(v["data"]["ready_to_work"], false);
}

#[tokio::test]
async fn dry_run_byow_skips_registration_and_env_write() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;
    // Deliberately no /agents/me/wallet mock — a hit would 404 and fail the test.

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let mut args = base_args();
    args.wallet_address = Some(BYOW_ADDRESS.to_string());

    let envelope = run(
        &ctx_for(&server, Some("test-key"), cfg_path.clone(), true),
        args,
    )
    .await
    .expect("dry-run should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["wallet"]["status"], "skipped");
    assert_eq!(v["data"]["config_file"]["written"], false);
    assert_eq!(v["data"]["config_file"]["would_write"], true);
    assert!(!cfg_path.exists(), "dry-run must not write config file");
}

#[tokio::test]
async fn inactive_agent_status_surfaces_validation_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "alice",
            "status": "suspended",
            "capabilities": [],
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let args = base_args();

    let err = run(
        &ctx_for(&server, Some("test-key"), config_path_in(tmp.path()), false),
        args,
    )
    .await
    .expect_err("suspended → Validation");

    match err {
        CmdError::Validation { code, .. } => assert_eq!(code, "agent_not_active"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn init_rejects_unknown_agent_status_as_not_active() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "alice",
            "status": "suspended_pending_review",
            "capabilities": [],
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let err = run(
        &ctx_for(&server, Some("test-key"), cfg_path.clone(), false),
        base_args(),
    )
    .await
    .expect_err("unknown status must fail closed before provisioning");

    match err {
        CmdError::Validation { code, message } => {
            assert_eq!(code, "agent_not_active");
            assert!(
                message.contains("suspended_pending_review"),
                "message must retain server status: {message}"
            );
        }
        other => panic!("expected Validation, got {other:?}"),
    }
    assert!(!cfg_path.exists(), "inactive agent must not write config");
    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1, "must stop after profile validation");
}

#[tokio::test]
async fn api_key_falls_back_to_config_when_ctx_is_empty() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let seed = Config {
        api_key: Some("from-config".into()),
        ..Config::default()
    };
    seed.save(&cfg_path).unwrap();

    let args = base_args();
    let envelope = run(&ctx_for(&server, None, cfg_path.clone(), false), args)
        .await
        .expect("should pick up api_key from config");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);

    // Re-saved config still carries that key.
    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(loaded.api_key.as_deref(), Some("from-config"));
}

#[tokio::test]
async fn missing_api_key_everywhere_errors_cleanly() {
    // Before gh#238 a keyless run stopped here. Now it first tries
    // `POST /new`; against a deployment that does not offer the route
    // (the switch is off: 404) it ends on the very same error and exit
    // code, so orchestrators keyed on `missing_api_key` keep working.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": "not_found",
            "message": "The requested resource does not exist",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let args = keyless_args();

    let err = run(&ctx_for(&server, None, cfg_path.clone(), false), args)
        .await
        .expect_err("no key anywhere + route off → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
    assert!(!cfg_path.exists(), "nothing may be saved on a refusal");
    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1, "only POST /new, exactly once");
}

#[tokio::test]
async fn generate_wallet_without_password_errors_as_usage() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;

    let tmp = TempDir::new().unwrap();
    let mut args = base_args();
    args.generate_wallet = true;
    // No wallet_password_file; no TASKFAST_WALLET_PASSWORD env var.
    // (Tests run with whatever env the harness has; the env var is unset
    // in CI. If a developer has it set locally, clear it or skip.)
    let _ = std::env::var("TASKFAST_WALLET_PASSWORD").map(|_| {
        eprintln!("note: TASKFAST_WALLET_PASSWORD is set in the harness env; skipping");
    });
    if std::env::var("TASKFAST_WALLET_PASSWORD").is_ok() {
        return;
    }

    let err = run(
        &ctx_for(&server, Some("test-key"), config_path_in(tmp.path()), false),
        args,
    )
    .await
    .expect_err("missing password → Usage");
    match err {
        CmdError::Usage(msg) => {
            assert!(
                msg.contains("--wallet-password-file"),
                "message should mention the flag: {msg}"
            );
        }
        other => panic!("expected Usage, got {other:?}"),
    }
}

#[tokio::test]
async fn generate_wallet_with_password_file_persists_keystore_and_registers() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;

    // Accept any POST /agents/me/wallet (address is dynamic — freshly
    // generated signer) and echo it back. Registration carries no
    // readiness claim (server gh#1172); readiness comes from the
    // /agents/me/readiness mock above.
    Mock::given(method("POST"))
        .and(path("/agents/me/wallet"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tempo_wallet_address": "0x0000000000000000000000000000000000000000",
            "payout_method": "tempo_wallet",
            "payment_method": "tempo",
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let pw_path = tmp.path().join("pw");
    fs::write(&pw_path, "s3kret\n").unwrap();
    let keystore_path = tmp.path().join("wallet.json");

    let mut args = base_args();
    args.generate_wallet = true;
    args.wallet_password_file = Some(pw_path);
    args.keystore_path = Some(keystore_path.clone());

    let envelope = run(
        &ctx_for(&server, Some("test-key"), cfg_path.clone(), false),
        args,
    )
    .await
    .expect("generate path should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["wallet"]["status"], "generated");
    let addr = v["data"]["wallet"]["address"]
        .as_str()
        .expect("address string");
    assert!(addr.starts_with("0x") && addr.len() == 42, "addr: {addr}");
    assert_eq!(
        v["data"]["wallet"]["keystore_path"],
        keystore_path.display().to_string()
    );

    assert!(keystore_path.exists(), "keystore must be written");

    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(
        loaded.keystore_path.as_deref(),
        Some(keystore_path.as_path())
    );
    assert_eq!(
        loaded.wallet_address.as_deref().map(str::to_lowercase),
        Some(addr.to_lowercase())
    );
}

// ---- am-z58: --human-api-key headless mint path ----

const MINTED_AGENT_ID: &str = "11111111-1111-1111-1111-111111111111";
const MINTED_KEY: &str = "am_live_minted_0123456789abcdef";

#[tokio::test]
async fn human_api_key_mints_agent_then_continues_with_returned_key() {
    let server = MockServer::start().await;

    // gh#238: a PAT is preferred over the no-credential route, which must
    // not be touched — every `POST /new` spends the caller's daily quota.
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(201).set_body_json(new_agent_body()))
        .expect(0)
        .mount(&server)
        .await;

    // POST /agents — PAT-authed mint. Body carries the required fields;
    // owner_id is derived server-side from the PAT, so the CLI omits it.
    Mock::given(method("POST"))
        .and(path("/agents"))
        .and(body_partial_json(json!({
            "name": "taskfast-agent",
            "capabilities": ["general"],
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": MINTED_AGENT_ID,
            "api_key": MINTED_KEY,
            "name": "taskfast-agent",
            "status": "active",
        })))
        .mount(&server)
        .await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let mut args = base_args();
    args.human_api_key = Some("tf_user_test".into());

    // Ctx carries no agent key — forces the mint branch.
    let envelope = run(&ctx_for(&server, None, cfg_path.clone(), false), args)
        .await
        .expect("mint path should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["agent"]["action"], "minted");
    assert_eq!(v["data"]["agent"]["id"], MINTED_AGENT_ID);

    // Config carries the minted agent key, not the PAT.
    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(loaded.api_key.as_deref(), Some(MINTED_KEY));
    assert_eq!(loaded.agent_id.as_deref(), Some(MINTED_AGENT_ID));
}

#[tokio::test]
async fn dry_run_human_api_key_reports_would_mint_and_skips_post() {
    let server = MockServer::start().await;
    // Any HTTP hit would 404 and the envelope would be an error, not ok.
    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let mut args = base_args();
    args.human_api_key = Some("tf_user_test".into());

    let envelope = run(&ctx_for(&server, None, cfg_path.clone(), true), args)
        .await
        .expect("dry-run mint should succeed without HTTP");

    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["agent"]["action"], "would_mint");
    assert!(
        v["data"]["agent"].get("owner_id").is_none(),
        "dry-run envelope should not carry owner_id — it is server-derived"
    );
    assert_eq!(v["data"]["config_file"]["written"], false);
    assert_eq!(v["data"]["config_file"]["would_write"], true);
    assert!(!cfg_path.exists(), "dry-run must not write config file");
}

#[tokio::test]
async fn existing_config_key_takes_precedence_over_human_api_key() {
    // Idempotency: re-running init with --human-api-key but a config-file
    // key must NOT mint a second agent.
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;
    // Deliberately no POST /agents mock — a hit would 404.

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let seed = Config {
        api_key: Some("am_live_already_have_one".into()),
        ..Config::default()
    };
    seed.save(&cfg_path).unwrap();

    let mut args = base_args();
    args.human_api_key = Some("tf_user_should_be_ignored".into());

    let envelope = run(&ctx_for(&server, None, cfg_path.clone(), false), args)
        .await
        .expect("idempotent path");
    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    // agent field is only set on mint — absence means we didn't mint.
    assert!(v["data"].get("agent").is_none(), "should not have minted");
}

// ---- gh#238: no key of any kind → POST /new ----

const NEW_AGENT_ID: &str = "22222222-2222-2222-2222-222222222222";
const NEW_AGENT_KEY: &str = "am_live_unclaimed_0123456789abcdef";
const CLAIM_URL: &str = "https://staging.taskfast.app/new/claim?claim_token=tok";
const EXPIRES_AT: &str = "2026-11-26T17:48:51Z";

/// The body `NewAgentJSON.created/1` sends, field for field.
fn new_agent_body() -> serde_json::Value {
    json!({
        "agent_id": NEW_AGENT_ID,
        "api_key": NEW_AGENT_KEY,
        "claim_url": CLAIM_URL,
        "expires_at": EXPIRES_AT,
        "terms_accepted": true,
        "terms_url": "https://staging.taskfast.app/terms/agents",
        "terms_version": "2026-11-15",
        "terms_notice": "By sending this request you accepted the TaskFast Terms of Service at https://staging.taskfast.app/terms/agents.",
        "wallet_required": true,
        "quick_start": "Set a wallet with POST /api/agents/me/wallet, fund it, then prepare and submit a task draft.",
        "links": {
            "mcp": "https://staging.taskfast.app/mcp",
            "openapi": "https://staging.taskfast.app/api/docs/spec",
            "tasks": "https://staging.taskfast.app/api/tasks",
            "wallet": "https://staging.taskfast.app/api/agents/me/wallet",
        },
    })
}

/// `POST /new` answering 201, matched on the flags' default name and
/// description so the body the CLI sends is pinned too.
async fn mount_post_new(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/new"))
        .and(body_partial_json(json!({
            "name": "taskfast-agent",
            "description": "Headless agent registered via taskfast init",
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(new_agent_body()))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn no_key_creates_an_unclaimed_agent_once_and_continues_with_its_key() {
    let server = MockServer::start().await;
    mount_post_new(&server).await;

    // Everything after the mint authenticates with the key POST /new
    // returned — pinned by the header matcher, not just by the outcome.
    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .and(header("x-api-key", NEW_AGENT_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": NEW_AGENT_ID,
            "name": "taskfast-agent",
            "status": "active",
            "capabilities": [],
        })))
        .mount(&server)
        .await;
    mount_readiness(&server, "complete", true).await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());

    let envelope = run(
        &ctx_for(&server, None, cfg_path.clone(), false),
        keyless_args(),
    )
    .await
    .expect("POST /new path should succeed");
    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);

    // The claim half of the response is part of the machine contract.
    assert_eq!(v["data"]["agent"]["action"], "minted_unclaimed");
    assert_eq!(v["data"]["agent"]["id"], NEW_AGENT_ID);
    assert_eq!(v["data"]["agent"]["name"], "taskfast-agent");
    assert_eq!(v["data"]["agent"]["claim_url"], CLAIM_URL);
    assert_eq!(v["data"]["agent"]["expires_at"], EXPIRES_AT);
    assert_eq!(v["data"]["agent"]["terms_version"], "2026-11-15");
    assert_eq!(v["data"]["agent"]["terms_accepted"], true);
    assert_eq!(v["data"]["config_file"]["written"], true);

    // The key AND the way to claim it are saved together.
    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(loaded.api_key.as_deref(), Some(NEW_AGENT_KEY));
    assert_eq!(loaded.agent_id.as_deref(), Some(NEW_AGENT_ID));
    assert_eq!(loaded.claim_url.as_deref(), Some(CLAIM_URL));
    assert_eq!(loaded.claim_expires_at.as_deref(), Some(EXPIRES_AT));

    let requests = server.received_requests().await.expect("recorded requests");
    let new_calls = requests.iter().filter(|r| r.url.path() == "/new").count();
    assert_eq!(new_calls, 1, "POST /new exactly once");
    let new_req = requests.iter().find(|r| r.url.path() == "/new").unwrap();
    assert!(
        new_req.headers.get("x-api-key").is_none(),
        "POST /new carries no credential"
    );
}

#[tokio::test]
async fn no_key_daily_limit_429_is_rate_limited_and_saves_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "86400")
                .set_body_json(json!({
                    "error": "rate_limited",
                    "message": "Too many new agents from this address; retry after the rate-limit window expires.",
                    "retry_after_seconds": 86400,
                })),
        )
        // Retrying would spend quota for nothing: one request, period.
        .expect(1)
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());

    let err = run(
        &ctx_for(&server, None, cfg_path.clone(), false),
        keyless_args(),
    )
    .await
    .expect_err("daily limit → RateLimited");
    assert!(
        matches!(err, CmdError::RateLimited { retry_after } if retry_after.as_secs() == 86_400),
        "got {err:?}"
    );
    assert!(!cfg_path.exists(), "nothing may be saved on a refusal");
}

#[tokio::test]
async fn no_key_terms_unpublished_503_is_a_server_error_and_saves_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": "try_later",
            "message": "The Terms of Service are not published yet; try again later.",
            "retry_after_seconds": 3600,
        })))
        // 5xx is retryable elsewhere in the client; not here.
        .expect(1)
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());

    let err = run(
        &ctx_for(&server, None, cfg_path.clone(), false),
        keyless_args(),
    )
    .await
    .expect_err("Terms unpublished → Server");
    match err {
        CmdError::Server(msg) => assert_eq!(
            msg,
            "The Terms of Service are not published yet; try again later."
        ),
        other => panic!("expected Server, got {other:?}"),
    }
    assert!(!cfg_path.exists(), "nothing may be saved on a refusal");
}

#[tokio::test]
async fn dry_run_without_a_key_previews_post_new_and_touches_nothing() {
    // No mocks at all: any HTTP hit would 404 and change the outcome.
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());

    let envelope = run(
        &ctx_for(&server, None, cfg_path.clone(), true),
        keyless_args(),
    )
    .await
    .expect("dry-run needs no server");
    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["agent"]["action"], "would_mint_unclaimed");
    assert_eq!(v["data"]["agent"]["name"], "taskfast-agent");
    assert_eq!(v["data"]["config_file"]["written"], false);
    assert_eq!(v["data"]["config_file"]["would_write"], true);
    assert!(!cfg_path.exists(), "dry-run must not write config file");
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "dry-run must not call POST /new"
    );
}

#[tokio::test]
async fn no_key_without_a_wallet_flag_stops_before_post_new() {
    // gh#238 review, P1: this run used to create an agent and then die in
    // the wallet step on this very error, throwing away its key and claim
    // link. Now the usage error comes first and no quota is spent.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(201).set_body_json(new_agent_body()))
        .expect(0)
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());

    let err = run(
        &ctx_for(&server, None, cfg_path.clone(), false),
        base_args(),
    )
    .await
    .expect_err("no wallet signal → Usage before any request");
    match err {
        CmdError::Usage(msg) => assert!(
            msg.contains("--generate-wallet") && msg.contains("--skip-wallet"),
            "message: {msg}"
        ),
        other => panic!("expected Usage, got {other:?}"),
    }
    assert!(!cfg_path.exists());
}

#[tokio::test]
async fn no_key_generate_wallet_without_a_password_stops_before_post_new() {
    // The second deterministic trigger from the review: the keystore
    // password is looked up before the request, not after the mint.
    if std::env::var("TASKFAST_WALLET_PASSWORD").is_ok() {
        eprintln!("note: TASKFAST_WALLET_PASSWORD is set in the harness env; skipping");
        return;
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(201).set_body_json(new_agent_body()))
        .expect(0)
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let mut args = base_args();
    args.generate_wallet = true;

    let err = run(&ctx_for(&server, None, cfg_path.clone(), false), args)
        .await
        .expect_err("--generate-wallet without a password → Usage before any request");
    match err {
        CmdError::Usage(msg) => {
            assert!(msg.contains("--wallet-password-file"), "message: {msg}")
        }
        other => panic!("expected Usage, got {other:?}"),
    }
    assert!(!cfg_path.exists());
}

#[tokio::test]
async fn unclaimed_key_and_claim_link_survive_a_later_failure_and_no_remint_happens() {
    // gh#238 review, P1, the non-deterministic case: `POST /new` answers
    // 201 and the very next call (GET /agents/me) fails with a 5xx. The
    // key and the claim link must already be on disk, and the re-run must
    // pick them up instead of minting a second agent against the quota.
    let server = MockServer::start().await;
    mount_post_new(&server).await; // expect(1): one mint across BOTH runs

    // First run: the profile read fails after the mint.
    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .and(header("x-api-key", NEW_AGENT_KEY))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "internal_error",
            "message": "database unavailable",
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Every later read succeeds with the saved key.
    Mock::given(method("GET"))
        .and(path("/agents/me"))
        .and(header("x-api-key", NEW_AGENT_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": NEW_AGENT_ID,
            "name": "taskfast-agent",
            "status": "active",
            "capabilities": [],
        })))
        .mount(&server)
        .await;
    mount_readiness(&server, "complete", true).await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());

    let err = run(
        &ctx_for(&server, None, cfg_path.clone(), false),
        keyless_args(),
    )
    .await
    .expect_err("the 5xx after the mint still fails the run");
    assert!(matches!(err, CmdError::Server(_)), "got {err:?}");

    // …but the agent is not stranded: key and claim link are on disk.
    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(loaded.api_key.as_deref(), Some(NEW_AGENT_KEY));
    assert_eq!(loaded.agent_id.as_deref(), Some(NEW_AGENT_ID));
    assert_eq!(loaded.claim_url.as_deref(), Some(CLAIM_URL));
    assert_eq!(loaded.claim_expires_at.as_deref(), Some(EXPIRES_AT));

    // The re-run finds that key and continues as a pre-existing agent.
    let envelope = run(
        &ctx_for(&server, None, cfg_path.clone(), false),
        keyless_args(),
    )
    .await
    .expect("re-run continues from the saved key");
    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert!(
        v["data"].get("agent").is_none(),
        "the re-run must not mint again: {:?}",
        v["data"]["agent"]
    );
    // `mount_post_new` carries expect(1) and verifies on server drop.
}

#[tokio::test]
async fn impossible_agent_name_fails_before_post_new_is_tried() {
    // The spec's own length limit (carried by the generated type) is
    // checked first, so a bad flag never spends the daily quota.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(201).set_body_json(new_agent_body()))
        .expect(0)
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let mut args = base_args();
    args.agent_name = String::new();

    match run(&ctx_for(&server, None, cfg_path.clone(), false), args).await {
        Err(CmdError::Usage(msg)) => {
            assert!(msg.contains("--agent-name"), "message: {msg}")
        }
        other => panic!("expected Usage, got {other:?}"),
    }
    assert!(!cfg_path.exists());
}

#[tokio::test]
async fn minting_with_a_pat_clears_a_stale_claim_link() {
    // A leftover claim link from an earlier unclaimed init would point at
    // an agent this config no longer uses.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/agents"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": MINTED_AGENT_ID,
            "api_key": MINTED_KEY,
            "name": "taskfast-agent",
            "status": "active",
        })))
        .mount(&server)
        .await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;

    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    Config {
        claim_url: Some("https://staging.taskfast.app/new/claim?claim_token=old".into()),
        claim_expires_at: Some("2026-01-01T00:00:00Z".into()),
        ..Config::default()
    }
    .save(&cfg_path)
    .unwrap();
    let mut args = base_args();
    args.human_api_key = Some("tf_user_test".into());

    run(&ctx_for(&server, None, cfg_path.clone(), false), args)
        .await
        .expect("PAT mint path");
    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(loaded.agent_id.as_deref(), Some(MINTED_AGENT_ID));
    assert_eq!(loaded.claim_url, None, "stale claim link must go");
    assert_eq!(loaded.claim_expires_at, None);
}

#[tokio::test]
async fn key_from_flag_or_env_clears_the_config_claim_link_of_another_agent() {
    // gh#238 review: `TASKFAST_API_KEY=<B>` on a config that still holds
    // agent A's key and claim link must not save (B, A's link).
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;
    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    Config {
        api_key: Some("am_live_agent_A".into()),
        claim_url: Some("https://staging.taskfast.app/new/claim?claim_token=A".into()),
        claim_expires_at: Some("2026-11-26T17:48:51Z".into()),
        ..Config::default()
    }
    .save(&cfg_path)
    .unwrap();
    let mut args = base_args();
    args.skip_wallet = true;

    run(
        &ctx_for(&server, Some("am_live_agent_B"), cfg_path.clone(), false),
        args,
    )
    .await
    .expect("pre-existing key path");
    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(loaded.api_key.as_deref(), Some("am_live_agent_B"));
    assert_eq!(loaded.claim_url, None, "A's link does not belong to B");
    assert_eq!(loaded.claim_expires_at, None);
}

#[tokio::test]
async fn key_from_the_config_keeps_its_own_claim_link() {
    // …while a re-run on the saved key keeps the pair together.
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;
    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    Config {
        api_key: Some(NEW_AGENT_KEY.into()),
        claim_url: Some(CLAIM_URL.into()),
        claim_expires_at: Some(EXPIRES_AT.into()),
        ..Config::default()
    }
    .save(&cfg_path)
    .unwrap();
    let mut args = base_args();
    args.skip_wallet = true;

    run(&ctx_for(&server, None, cfg_path.clone(), false), args)
        .await
        .expect("re-run on the saved key");
    let loaded = Config::load(&cfg_path).unwrap();
    assert_eq!(loaded.api_key.as_deref(), Some(NEW_AGENT_KEY));
    assert_eq!(loaded.claim_url.as_deref(), Some(CLAIM_URL));
    assert_eq!(loaded.claim_expires_at.as_deref(), Some(EXPIRES_AT));
}

#[tokio::test]
async fn blank_agent_name_stops_before_post_new() {
    // Whitespace passes the spec's length limits; the server trims and
    // refuses — after counting the request. Mirrored before the request.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/new"))
        .respond_with(ResponseTemplate::new(201).set_body_json(new_agent_body()))
        .expect(0)
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let cfg_path = config_path_in(tmp.path());
    let mut args = keyless_args();
    args.agent_name = "   ".into();

    match run(&ctx_for(&server, None, cfg_path.clone(), false), args).await {
        Err(CmdError::Usage(msg)) => assert!(msg.contains("--agent-name"), "message: {msg}"),
        other => panic!("expected Usage, got {other:?}"),
    }
    assert!(!cfg_path.exists());
}

#[tokio::test]
async fn created_201_without_an_api_key_is_a_server_error_and_saves_nothing() {
    // Sibling of create_agent_headless's invariant: the key is shown once,
    // so a 201 that omits it cannot be proceeded from. Checked before the
    // early save, so nothing reaches the config.
    for body in [
        {
            let mut b = new_agent_body();
            b.as_object_mut().unwrap().remove("api_key");
            b
        },
        {
            let mut b = new_agent_body();
            b["api_key"] = json!("");
            b
        },
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/new"))
            .respond_with(ResponseTemplate::new(201).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let tmp = TempDir::new().unwrap();
        let cfg_path = config_path_in(tmp.path());

        let err = run(
            &ctx_for(&server, None, cfg_path.clone(), false),
            keyless_args(),
        )
        .await
        .expect_err("201 without a usable key");
        match err {
            CmdError::Server(msg) => {
                assert_eq!(msg, "POST /new returned no api_key despite 201")
            }
            other => panic!("expected Server, got {other:?}"),
        }
        assert!(!cfg_path.exists(), "nothing may be saved without a key");
    }
}

#[tokio::test]
async fn unfunded_wallet_at_init_end_surfaces_machine_signals_and_stays_ok() {
    // gh#142: faucet failure / zero balance must not sit quietly next to
    // success. The stderr warning text itself is unit-tested against
    // `build_funding_warning`; here we pin the machine-readable contract —
    // the run stays ok:true (a warning, not a failure) while the envelope
    // carries the server's `funded` gate + `funding_hint`, and the
    // end-of-init balance check actually consulted the balance endpoint
    // (zero) and the advertised rpc_url for the fallback hint.
    let server = MockServer::start().await;
    mount_profile_active(&server).await;

    Mock::given(method("GET"))
        .and(path("/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ready_to_work": false,
            "checks": {
                "api_key": {"status": "complete"},
                "wallet": {"status": "complete"},
                "funded": {
                    "status": "unfunded",
                    "funding_hint": "https://wallet.tempo.xyz/",
                },
                "webhook": {"status": "not_configured", "required": false},
            },
            "settlement_domain": {
                "chain_id": 42431,
                "verifying_contract": "0x0000000000000000000000000000000000000000",
            },
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/agents/me/wallet"))
        .and(body_partial_json(json!({
            "tempo_wallet_address": BYOW_ADDRESS,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tempo_wallet_address": BYOW_ADDRESS,
            "payout_method": "tempo_wallet",
            "payment_method": "tempo",
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/agents/me/wallet/balance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chain": "tempo",
            "wallet_address": BYOW_ADDRESS,
            "available_balance": "0x0",
            "currency": "USDC",
            "checked_at": "2026-09-07T12:00:00Z",
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/config/network"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "networks": {
                "testnet": {
                    "chain_id": 42431,
                    "rpc_url": format!("{}/rpc", server.uri()),
                    "wss_url": null,
                    "explorer_url": null,
                },
            },
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let mut args = base_args();
    args.wallet_address = Some(BYOW_ADDRESS.to_string());

    let envelope = run(
        &ctx_for(&server, Some("test-key"), config_path_in(tmp.path()), false),
        args,
    )
    .await
    .expect("init stays ok:true — funding gap is a warning, not a failure");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["faucet"]["status"], "skipped");
    assert_eq!(v["data"]["ready_to_work"], false);
    assert_eq!(
        v["data"]["readiness"]["checks"]["funded"]["status"],
        "unfunded"
    );
    assert_eq!(
        v["data"]["readiness"]["checks"]["funded"]["funding_hint"],
        "https://wallet.tempo.xyz/"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn init_refuses_unwritable_config_path_before_any_request() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let shared = tmp.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::os::unix::fs::symlink(&shared, tmp.path().join(".taskfast")).unwrap();

    let err = run(
        &ctx_for(&server, Some("test-key"), config_path_in(tmp.path()), false),
        keyless_args(),
    )
    .await
    .expect_err("symlinked .taskfast must be refused");
    assert!(err.to_string().contains("symlink"), "{err}");
    assert!(server.received_requests().await.unwrap().is_empty());
}
