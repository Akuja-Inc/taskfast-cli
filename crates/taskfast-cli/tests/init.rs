//! End-to-end tests for `taskfast init`.
//!
//! Covers the full command pipeline (api-key resolution, validate,
//! readiness, wallet provisioning, env-file persistence, final readiness)
//! against a wiremock server.

use std::fs;
use std::path::PathBuf;

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use taskfast_cli::cmd::init::{Args, Network, run};
use taskfast_cli::cmd::{CmdError, Ctx};
use taskfast_cli::dotenv::EnvFile;
use taskfast_cli::{Environment, Envelope};

const BYOW_ADDRESS: &str = "0xdEaDbEeF00000000000000000000000000000001";

fn ctx_for(server: &MockServer, key: Option<&str>, dry_run: bool) -> Ctx {
    Ctx {
        api_key: key.map(String::from),
        environment: Environment::Local,
        api_base: Some(server.uri()),
        dry_run,
        quiet: true,
    }
}

fn base_args(env_file: PathBuf) -> Args {
    Args {
        wallet_address: None,
        generate_wallet: false,
        wallet_password_file: None,
        keystore_path: None,
        network: Network::Testnet,
        env_file: Some(env_file),
        skip_wallet: false,
    }
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

async fn mount_readiness(
    server: &MockServer,
    wallet_status: &str,
    ready_to_work: bool,
) {
    Mock::given(method("GET"))
        .and(path("/agents/me/readiness"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ready_to_work": ready_to_work,
            "checks": {
                "api_key": {"status": "complete"},
                "wallet": {"status": wallet_status},
                "webhook": {"status": "not_configured", "required": false},
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
            "ready_to_work": true,
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".taskfast-agent.env");
    let mut args = base_args(env_path.clone());
    args.wallet_address = Some(BYOW_ADDRESS.to_string());

    let envelope = run(&ctx_for(&server, Some("test-key"), false), args)
        .await
        .expect("init should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["wallet"]["status"], "byo_registered");
    assert_eq!(v["data"]["wallet"]["address"], BYOW_ADDRESS);
    assert_eq!(v["data"]["env_file"]["written"], true);

    // Env file exists and carries the registered address + api key.
    let loaded = EnvFile::load(&env_path).unwrap();
    assert_eq!(loaded.get("TASKFAST_API_KEY"), Some("test-key"));
    assert_eq!(loaded.get("TEMPO_WALLET_ADDRESS"), Some(BYOW_ADDRESS));
    assert_eq!(loaded.get("TEMPO_NETWORK"), Some("testnet"));
    assert_eq!(loaded.get("TASKFAST_API").unwrap(), &server.uri());
}

#[tokio::test]
async fn skips_wallet_when_server_already_has_one() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;

    let tmp = TempDir::new().unwrap();
    let args = base_args(tmp.path().join(".env"));

    let envelope = run(&ctx_for(&server, Some("test-key"), false), args)
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
    let mut args = base_args(tmp.path().join(".env"));
    args.skip_wallet = true;

    let envelope = run(&ctx_for(&server, Some("test-key"), false), args)
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
    let env_path = tmp.path().join(".env");
    let mut args = base_args(env_path.clone());
    args.wallet_address = Some(BYOW_ADDRESS.to_string());

    let envelope = run(&ctx_for(&server, Some("test-key"), true), args)
        .await
        .expect("dry-run should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["data"]["wallet"]["status"], "skipped");
    assert_eq!(v["data"]["env_file"]["written"], false);
    assert_eq!(v["data"]["env_file"]["would_write"], true);
    assert!(!env_path.exists(), "dry-run must not write env file");
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
    let args = base_args(tmp.path().join(".env"));

    let err = run(&ctx_for(&server, Some("test-key"), false), args)
        .await
        .expect_err("suspended → Validation");

    match err {
        CmdError::Validation { code, .. } => assert_eq!(code, "agent_not_active"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[tokio::test]
async fn api_key_falls_back_to_env_file_when_ctx_is_empty() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "complete", true).await;

    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    let mut seed = EnvFile::new();
    seed.set("TASKFAST_API_KEY", "from-env-file");
    seed.save(&env_path).unwrap();

    let args = base_args(env_path.clone());
    let envelope = run(&ctx_for(&server, None, false), args)
        .await
        .expect("should pick up api_key from env file");

    let v = envelope_value(&envelope);
    assert_eq!(v["ok"], true);

    // Re-saved env file still carries that key.
    let loaded = EnvFile::load(&env_path).unwrap();
    assert_eq!(loaded.get("TASKFAST_API_KEY"), Some("from-env-file"));
}

#[tokio::test]
async fn missing_api_key_everywhere_errors_cleanly() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let args = base_args(tmp.path().join(".env"));

    let err = run(&ctx_for(&server, None, false), args)
        .await
        .expect_err("no key anywhere → MissingApiKey");
    assert!(matches!(err, CmdError::MissingApiKey), "got {err:?}");
}

#[tokio::test]
async fn generate_wallet_without_password_errors_as_usage() {
    let server = MockServer::start().await;
    mount_profile_active(&server).await;
    mount_readiness(&server, "missing", false).await;

    let tmp = TempDir::new().unwrap();
    let mut args = base_args(tmp.path().join(".env"));
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

    let err = run(&ctx_for(&server, Some("test-key"), false), args)
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
    // generated signer) and echo it back.
    Mock::given(method("POST"))
        .and(path("/agents/me/wallet"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tempo_wallet_address": "0x0000000000000000000000000000000000000000",
            "payout_method": "tempo_wallet",
            "payment_method": "tempo",
            "ready_to_work": true,
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    let pw_path = tmp.path().join("pw");
    fs::write(&pw_path, "s3kret\n").unwrap();
    let keystore_path = tmp.path().join("wallet.json");

    let mut args = base_args(env_path.clone());
    args.generate_wallet = true;
    args.wallet_password_file = Some(pw_path);
    args.keystore_path = Some(keystore_path.clone());

    let envelope = run(&ctx_for(&server, Some("test-key"), false), args)
        .await
        .expect("generate path should succeed");

    let v = envelope_value(&envelope);
    assert_eq!(v["data"]["wallet"]["status"], "generated");
    let addr = v["data"]["wallet"]["address"]
        .as_str()
        .expect("address string");
    assert!(addr.starts_with("0x") && addr.len() == 42, "addr: {addr}");
    assert_eq!(v["data"]["wallet"]["keystore_path"], keystore_path.display().to_string());

    assert!(keystore_path.exists(), "keystore must be written");

    let loaded = EnvFile::load(&env_path).unwrap();
    assert_eq!(
        loaded.get("TEMPO_KEY_SOURCE"),
        Some(format!("file:{}", keystore_path.display()).as_str())
    );
    assert_eq!(loaded.get("TEMPO_WALLET_ADDRESS").map(str::to_lowercase), Some(addr.to_lowercase()));
}
