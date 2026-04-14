//! E2E sanity: `config set` then `config show` round-trips through the
//! same JSON file, and redaction survives that round-trip.

use tempfile::TempDir;

use taskfast_cli::cmd::config::{run, Command, SetArgs, ShowArgs};
use taskfast_cli::cmd::Ctx;
use taskfast_cli::{Envelope, Environment};

fn ctx_for(path: std::path::PathBuf) -> Ctx {
    Ctx {
        api_key: None,
        environment: Environment::Local,
        api_base: None,
        config_path: path,
        dry_run: false,
        quiet: true,
    }
}

fn env_value(e: &Envelope) -> serde_json::Value {
    serde_json::to_value(e).expect("serialize envelope")
}

#[tokio::test]
async fn set_then_show_roundtrips_and_redacts_api_key() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join(".taskfast").join("config.json");
    let ctx = ctx_for(path.clone());

    // Write two fields — one secret, one not.
    for (k, v) in [
        ("api_key", "am_live_roundtrip9999"),
        ("network", "testnet"),
    ] {
        run(
            &ctx,
            Command::Set(SetArgs {
                key: k.into(),
                value: Some(v.into()),
                unset: false,
            }),
        )
        .await
        .unwrap();
    }

    // Show with default (redacted) api_key.
    let env = run(&ctx, Command::Show(ShowArgs { reveal: false }))
        .await
        .unwrap();
    let v = env_value(&env);
    assert_eq!(v["data"]["config"]["api_key"], "***9999");
    assert_eq!(v["data"]["config"]["network"], "testnet");
    assert_eq!(v["data"]["path"], path.display().to_string());

    // Show --reveal prints the full key.
    let env = run(&ctx, Command::Show(ShowArgs { reveal: true }))
        .await
        .unwrap();
    let v = env_value(&env);
    assert_eq!(v["data"]["config"]["api_key"], "am_live_roundtrip9999");
}

#[tokio::test]
async fn path_before_and_after_creation() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join(".taskfast").join("config.json");
    let ctx = ctx_for(path.clone());

    let env = run(&ctx, Command::Path).await.unwrap();
    let v = env_value(&env);
    assert_eq!(v["data"]["exists"], false);

    run(
        &ctx,
        Command::Set(SetArgs {
            key: "network".into(),
            value: Some("mainnet".into()),
            unset: false,
        }),
    )
    .await
    .unwrap();

    let env = run(&ctx, Command::Path).await.unwrap();
    let v = env_value(&env);
    assert_eq!(v["data"]["exists"], true);
    assert_eq!(v["data"]["path"], path.display().to_string());
}
