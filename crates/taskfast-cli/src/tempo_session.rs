// SPDX-License-Identifier: MIT
//! Deep Tempo chain-session module: the one seam every on-chain command goes
//! through to reach a chain and sign.
//!
//! [`ChainConnection::resolve`] picks the endpoint (an explicit override or the
//! deployment's proxy, by environment network or by a server-issued chain id —
//! L1 or a Tempo Zone venue) and enforces the endpoint guards and the
//! env→network invariant. [`ChainConnection::into_session`] then unlocks the
//! keystore, runs the wallet preflight, checks the node's `eth_chainId` (so
//! every tx is signed for the chain that was actually verified; skipped under
//! `--dry-run`) and picks the receipt timeout, yielding a [`TempoSession`]
//! that serializes every broadcast under the exclusive [`wallet_lock`].
//!
//! `MockTempoRpc` (feature `test-support`, off in release builds) is an
//! in-memory node: register one for an RPC URL with `register_mock_rpc` and
//! the real commands run against it end to end.

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(any(test, feature = "test-support"))]
use std::collections::{HashMap, HashSet};
#[cfg(any(test, feature = "test-support"))]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(any(test, feature = "test-support"))]
use std::sync::{Arc, Mutex, OnceLock};

use alloy_primitives::{Address, Bytes, TxHash, U256};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use serde_json::Value;

use taskfast_agent::chain::{decode_u256, IERC20};
use taskfast_agent::tempo_rpc::{
    sign_and_broadcast_tx, sign_and_broadcast_tx_for_chain, RpcError, TempoRpcClient,
};
#[cfg(any(test, feature = "test-support"))]
use taskfast_agent::tempo_rpc::{sign_legacy_tx, signed_tx_chain_id};
use taskfast_chains::tempo::{
    is_allowed_fee_token, is_known_network, TEMPO_MAINNET_CHAIN_ID, TEMPO_TESTNET_CHAIN_ID,
};
use taskfast_client::{NetworkConfigEntry, NetworkConfigResponse, TaskFastClient};

use crate::cmd::{resolve_duration, wallet_args, CmdError, Ctx};
use crate::wallet_lock::{self, WalletGuard};
use crate::{Environment, Network};

/// Default receipt timeouts and polling interval across all on-chain commands.
pub const DEFAULT_RECEIPT_TIMEOUT_MAINNET: Duration = Duration::from_mins(3);
pub const DEFAULT_RECEIPT_TIMEOUT_TESTNET: Duration = Duration::from_mins(1);
pub const RECEIPT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Default receipt timeout given a chain ID.
pub fn default_receipt_timeout_for_chain_id(chain_id: u64) -> Duration {
    if chain_id == TEMPO_MAINNET_CHAIN_ID {
        DEFAULT_RECEIPT_TIMEOUT_MAINNET
    } else {
        DEFAULT_RECEIPT_TIMEOUT_TESTNET
    }
}

/// Default receipt timeout given an abstract Network.
pub fn default_receipt_timeout_for_network(network: Network) -> Duration {
    match network {
        Network::Mainnet => DEFAULT_RECEIPT_TIMEOUT_MAINNET,
        Network::Testnet => DEFAULT_RECEIPT_TIMEOUT_TESTNET,
    }
}

/// Which chain a connection must reach, and where the expected chain id comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainTarget {
    /// A server-issued chain id (escrow params, dispute signing params, a task's
    /// settlement domain): the `GET /config/network` entry advertising it, L1 or
    /// a Tempo Zone venue. The node must be on that chain.
    ChainId(i64),
    /// The deployment's entry for this network. The node must be on the chain
    /// that entry advertises.
    Network(Network),
    /// No server-issued chain: the environment's entry supplies the default
    /// proxy route, and the node must be on the chain that entry advertises.
    /// With a `--rpc-url` override nothing pins the chain, and the node's own
    /// chain id is what gets signed for (the fee flow's allowlist is keyed by
    /// it).
    Node,
}

/// Wallet + RPC flags every signing command shares (flag, env, then `Ctx` config).
#[derive(Clone, Copy, Debug, Default)]
pub struct WalletOpts<'a> {
    /// Keystore reference (path, or the form `wallet_args` accepts).
    pub keystore: Option<&'a str>,
    /// File holding the keystore password.
    pub password_file: Option<&'a Path>,
    /// Preflight: fail if the keystore's address differs.
    pub wallet_address: Option<&'a str>,
    /// Explicit RPC override instead of the deployment's proxy.
    pub rpc_url: Option<&'a str>,
    /// How long to wait for a receipt (flag > config > chain-aware default).
    pub receipt_timeout: Option<Duration>,
}

/// True when `rpc_url` is served from the same origin (scheme + host + port)
/// as this deployment's `base_url` — i.e. it is this deployment's own
/// authenticated proxy, whatever path it is mounted at (`/rpc/…` or
/// `/api/rpc/…`). Used both to gate off-host RPC routing and to decide
/// whether the authenticated reqwest client (with the `X-API-Key` default
/// header) is required for the call.
///
/// The path is deliberately irrelevant: every route on `base_url` is a
/// TaskFast route that already accepts the key, so the only boundary that
/// matters is the host — we never leak the key off-host. `Url::origin()`
/// folds in the scheme, so an `https`→`http` downgrade is a different origin
/// and is correctly rejected.
pub(crate) fn is_proxy_rpc_url(rpc_url: &str, base_url: &str) -> bool {
    match (url::Url::parse(rpc_url), url::Url::parse(base_url)) {
        // Two opaque origins are never equal, so a match here is always a
        // genuine same-host tuple origin.
        (Ok(rpc), Ok(base)) => rpc.origin() == base.origin(),
        _ => false,
    }
}

fn is_loopback_url(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(u) => matches!(
            u.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        ),
        Err(_) => false,
    }
}

/// Guard on an explicit RPC override: needs `--allow-custom-endpoints`,
/// and never plain HTTP off loopback when the target is mainnet (MITM on a
/// mainnet RPC can observe the poster wallet and inflate gas price).
pub(crate) fn validate_override_rpc_url(
    url: &str,
    network: Network,
    allow_custom: bool,
) -> Result<&str, CmdError> {
    if !allow_custom {
        return Err(CmdError::Usage(format!(
            "refusing to use custom tempo_rpc_url {url:?}: pass \
             --allow-custom-endpoints (or set \
             TASKFAST_ALLOW_CUSTOM_ENDPOINTS=1) to override. The default \
             path routes RPC through `{{api_base}}/rpc/{{network}}` \
             which inherits the api_base endpoint guard."
        )));
    }

    let is_plain_http_mainnet =
        matches!(network, Network::Mainnet) && url.starts_with("http://") && !is_loopback_url(url);
    if is_plain_http_mainnet {
        return Err(CmdError::Usage(format!(
            "refusing plain-HTTP mainnet tempo_rpc_url {url:?}: MITM on a \
             mainnet RPC can observe the poster wallet and inflate gas \
             price. Use HTTPS or point at loopback for local testing."
        )));
    }

    Ok(url)
}

/// The reqwest client for a JSON-RPC URL: this deployment's authenticated
/// client (carrying `X-API-Key`) when the URL's origin is `api_base`'s, a
/// bare client otherwise, so the key never leaves the deployment. Selected
/// by origin, not by how the URL was resolved.
pub(crate) fn rpc_http_client(
    ctx: &Ctx,
    client: &TaskFastClient,
    rpc_url: &str,
) -> reqwest::Client {
    if is_proxy_rpc_url(rpc_url, ctx.base_url()) {
        client.http_client()
    } else {
        reqwest::Client::new()
    }
}

/// One entry of `GET /config/network`, selected for a [`ChainTarget`].
struct Selected {
    entry: NetworkConfigEntry,
    name: String,
    /// The chain the node must report; `None` when nothing server-side pins it.
    expected: Option<u64>,
}

fn chain_id_mismatch(message: impl Into<String>) -> CmdError {
    CmdError::Validation {
        code: "chain_id_mismatch".into(),
        message: message.into(),
    }
}

pub(crate) fn chain_id_u64(chain_id: i64, what: &str) -> Result<u64, CmdError> {
    u64::try_from(chain_id)
        .map_err(|_| CmdError::Decode(format!("{what} chain_id={chain_id} is negative")))
}

/// `fetch_network_config` failures keep their variant (`RateLimited`,
/// `Network`, …) instead of being flattened into `Server`; only the
/// deployment-fault cases gain the base URL for context.
fn config_err(ctx: &Ctx, e: taskfast_client::Error) -> CmdError {
    match e {
        // The endpoint is public infrastructure: an auth or validation refusal
        // there is a deployment fault, not a credential the operator can fix.
        taskfast_client::Error::Auth(_) | taskfast_client::Error::Validation { .. } => {
            CmdError::Server(format!("fetch network config from {}: {e}", ctx.base_url()))
        }
        taskfast_client::Error::Server(m) => {
            CmdError::Server(format!("fetch network config from {}: {m}", ctx.base_url()))
        }
        other => other.into(),
    }
}

/// Env→network invariant over an already-fetched `GET /config/network`
/// (AGENTS.md: a transitional client-side exception, kept here so it has one
/// home). Bypassed for [`Environment::Local`] and `--allow-custom-endpoints`.
///
/// **Default: warn-only.** Today's deployments still advertise multiple
/// networks in one response (server-side one-network-per-deployment tracked in
/// #62). Set `TASKFAST_STRICT_ENV_NETWORK=1` to fail-closed instead.
pub(crate) fn enforce_network_invariant(
    ctx: &Ctx,
    cfg: &NetworkConfigResponse,
) -> Result<(), CmdError> {
    if network_invariant_bypassed(ctx) {
        return Ok(());
    }
    let strict = std::env::var("TASKFAST_STRICT_ENV_NETWORK")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let mut advertised: Vec<String> = cfg.networks.keys().cloned().collect();
    advertised.sort();
    check_network_invariant(ctx.environment, ctx.base_url(), &advertised, strict)
}

/// Whether the env→network invariant is bypassed for this run, in which case
/// `GET /config/network` need not even be fetched for it.
pub(crate) fn network_invariant_bypassed(ctx: &Ctx) -> bool {
    ctx.allow_custom_endpoints || ctx.environment == Environment::Local
}

/// Pure-logic core of [`enforce_network_invariant`], split out so the strict /
/// warn branches are unit-testable without a wiremock server.
fn check_network_invariant(
    env: Environment,
    api_base: &str,
    advertised: &[String],
    strict: bool,
) -> Result<(), CmdError> {
    let expected = env.network().as_str();
    if advertised.len() == 1 && advertised[0] == expected {
        return Ok(());
    }
    if strict {
        return Err(CmdError::Server(format!(
            "deployment at {api_base} advertises networks {advertised:?}; env {} requires \
             exactly [{expected}]. Server-side fix tracked in issue #62. Unset \
             TASKFAST_STRICT_ENV_NETWORK or pass --allow-custom-endpoints to bypass.",
            env.as_str(),
        )));
    }
    tracing::warn!(
        api_base = %api_base,
        env = env.as_str(),
        expected = expected,
        advertised = ?advertised,
        "deployment advertises a different network shape than env requires; \
         server-side fix tracked in issue #62. Set TASKFAST_STRICT_ENV_NETWORK=1 \
         to fail-closed."
    );
    Ok(())
}

/// The deployment's entry for `network`, after the env→network invariant.
fn network_entry(
    ctx: &Ctx,
    cfg: &NetworkConfigResponse,
    network: Network,
) -> Result<Selected, CmdError> {
    enforce_network_invariant(ctx, cfg)?;
    let name = network.as_str();
    let entry = cfg.entry(name).map_err(|e| {
        CmdError::Server(format!(
            "deployment at {} does not advertise network `{name}`: {e}",
            ctx.base_url()
        ))
    })?;
    Ok(Selected {
        entry: entry.clone(),
        name: name.to_string(),
        expected: None,
    })
}

/// Unlock the keystore and run the wallet-address preflight: flag > env >
/// config for every input. `purpose` names the flow in the "missing
/// --keystore" error. Returns the resolved keystore path too, since the
/// session's wallet lock is keyed off it.
pub fn load_wallet(
    ctx: &Ctx,
    wallet: &WalletOpts<'_>,
    purpose: &str,
) -> Result<(PrivateKeySigner, Option<PathBuf>), CmdError> {
    let keystore_path =
        wallet_args::resolve_keystore_path(wallet.keystore, ctx.keystore_path.as_deref());
    let signer = wallet_args::load_signer_and_validate(
        keystore_path.as_deref(),
        wallet.password_file,
        wallet.wallet_address.or(ctx.wallet_address.as_deref()),
        purpose,
    )?;
    Ok((signer, keystore_path))
}

/// A chain endpoint chosen and guarded, before any node I/O.
#[derive(Clone, Debug)]
pub struct ChainConnection {
    backend: TempoRpcBackend,
    rpc_url: String,
    entry: Option<NetworkConfigEntry>,
    /// The chain the node must report when a server-issued or advertised
    /// value pins it; `None` trusts the node's own answer.
    expected: Option<u64>,
    /// The chain id the node reported and that every tx is signed for.
    chain_id: Option<u64>,
}

impl ChainConnection {
    /// Pick the RPC endpoint for `target` and run the endpoint guards.
    ///
    /// * `--rpc-url` override: the `validate_override_rpc_url` guard (needs
    ///   `--allow-custom-endpoints`; HTTPS on mainnet).
    /// * otherwise the selected entry's proxy URL, which must be same-host
    ///   with `api_base` so the API key never leaves the deployment.
    /// * [`ChainTarget::Node`] with an override consults no deployment at all,
    ///   so it needs no TaskFast credentials.
    /// * [`ChainTarget::ChainId`] always consults the deployment, override or
    ///   not, `--dry-run` included: a server-issued chain id is only fit to
    ///   sign for if the deployment advertises it, and never when it is (or
    ///   is advertised as) the opposite L1 network; dry-run still signs.
    ///
    /// Makes no node request; [`Self::into_session`] verifies the node unless
    /// `--dry-run`.
    pub async fn resolve(
        ctx: &Ctx,
        client: &TaskFastClient,
        rpc_override: Option<&str>,
        target: ChainTarget,
    ) -> Result<Self, CmdError> {
        // The endpoint guard is pure: refuse an unvetted override before any
        // request to the deployment.
        if let Some(url) = rpc_override {
            // Mainnet rules (HTTPS off loopback) whenever the run itself is
            // mainnet or the chain is the mainnet L1: a mainnet Tempo Zone's
            // chain id is not in the policy table, so the run's network has
            // to vouch for it.
            let policy = match target {
                ChainTarget::ChainId(chain_id)
                    if chain_id_u64(chain_id, "server-issued")? == TEMPO_MAINNET_CHAIN_ID =>
                {
                    Network::Mainnet
                }
                _ => ctx.environment.network(),
            };
            validate_override_rpc_url(url, policy, ctx.allow_custom_endpoints)?;
        }
        let selected: Option<Selected> = match target {
            // An explicit node is the whole answer: no deployment lookup.
            ChainTarget::Node if rpc_override.is_some() => None,
            _ => {
                let cfg = client
                    .fetch_network_config()
                    .await
                    .map_err(|e| config_err(ctx, e))?;
                match target {
                    ChainTarget::ChainId(chain_id) => {
                        enforce_network_invariant(ctx, cfg)?;
                        let expected = chain_id_u64(chain_id, "server-issued")?;
                        let network = ctx.environment.network();
                        // Never the opposite L1 network, by chain id or by
                        // the name of the entry advertising it. That is all
                        // the CLI can tell apart: a Tempo Zone venue has its
                        // own chain id that says which L1 it sits on only
                        // through the deployment's entry for it.
                        let opposite_l1 = match network {
                            Network::Mainnet => ("testnet", TEMPO_TESTNET_CHAIN_ID),
                            Network::Testnet => ("mainnet", TEMPO_MAINNET_CHAIN_ID),
                        };
                        if expected == opposite_l1.1 {
                            return Err(chain_id_mismatch(format!(
                                "the server issued chain_id={chain_id}, the opposite L1 \
                                 network's chain, for a `{}` run; refusing to sign or send",
                                network.as_str()
                            )));
                        }
                        // The configured network's own entry when it is the one
                        // on that chain; otherwise whoever advertises the chain.
                        Some(match cfg.entry(network.as_str()) {
                            Ok(entry) if entry.chain_id == chain_id => Selected {
                                entry: entry.clone(),
                                name: network.as_str().to_string(),
                                expected: Some(expected),
                            },
                            _ => {
                                let (name, entry) =
                                    cfg.entry_by_chain_id(chain_id).map_err(|_| {
                                        chain_id_mismatch(format!(
                                        "the server issued chain_id={chain_id} but the deployment \
                                         at {} advertises no such chain; refusing to sign or send",
                                        ctx.base_url()
                                    ))
                                    })?;
                                if name == opposite_l1.0 {
                                    return Err(chain_id_mismatch(format!(
                                        "the server issued chain_id={chain_id}, advertised as \
                                         `{name}` by the deployment at {}: the opposite L1 network \
                                         for a `{}` run; refusing to sign or send",
                                        ctx.base_url(),
                                        network.as_str()
                                    )));
                                }
                                Selected {
                                    entry: entry.clone(),
                                    name: name.to_string(),
                                    expected: Some(expected),
                                }
                            }
                        })
                    }
                    ChainTarget::Network(network) => {
                        let s = network_entry(ctx, cfg, network)?;
                        let expected = chain_id_u64(s.entry.chain_id, "advertised")?;
                        Some(Selected {
                            expected: Some(expected),
                            ..s
                        })
                    }
                    // Only the proxy route is consulted, but through it the
                    // node must still be on the chain the entry advertises:
                    // a proxy misrouted to another network's node (same token
                    // addresses, real funds) must not pass.
                    ChainTarget::Node => {
                        let s = network_entry(ctx, cfg, ctx.environment.network())?;
                        let expected = chain_id_u64(s.entry.chain_id, "advertised")?;
                        Some(Selected {
                            expected: Some(expected),
                            ..s
                        })
                    }
                }
            }
        };
        let rpc_url = match (rpc_override, selected.as_ref()) {
            (Some(url), _) => url.to_string(),
            (None, Some(s)) => {
                if !is_proxy_rpc_url(&s.entry.rpc_url, ctx.base_url()) {
                    return Err(CmdError::Server(format!(
                        "deployment at {} returned rpc_url {:?} for network `{}`, which is not on \
                         the same host. Refusing to route RPC traffic off-host.",
                        ctx.base_url(),
                        s.entry.rpc_url,
                        s.name
                    )));
                }
                s.entry.rpc_url.clone()
            }
            // `selected` is `None` only for `Node` + override, matched above.
            (None, None) => {
                return Err(CmdError::Usage(
                    "no RPC endpoint: pass --rpc-url or run against a deployment".into(),
                ));
            }
        };
        let backend = TempoRpcBackend::for_url(ctx, client, &rpc_url);
        Ok(Self {
            backend,
            rpc_url,
            entry: selected.as_ref().map(|s| s.entry.clone()),
            expected: selected.and_then(|s| s.expected),
            chain_id: None,
        })
    }

    /// [`Self::resolve`] followed by [`Self::verify_node`]: the live path.
    pub async fn connect(
        ctx: &Ctx,
        client: &TaskFastClient,
        rpc_override: Option<&str>,
        target: ChainTarget,
    ) -> Result<Self, CmdError> {
        Self::resolve(ctx, client, rpc_override, target)
            .await?
            .verify_node()
            .await
    }

    /// An explicit `--rpc-url` with no deployment behind it: works without
    /// TaskFast credentials, and nothing server-side pins the chain.
    pub fn open_override(ctx: &Ctx, url: &str) -> Result<Self, CmdError> {
        validate_override_rpc_url(url, ctx.environment.network(), ctx.allow_custom_endpoints)?;
        let backend = TempoRpcBackend::for_override(ctx, url)?;
        Ok(Self {
            backend,
            rpc_url: url.to_string(),
            entry: None,
            expected: None,
            chain_id: None,
        })
    }

    /// RPC URL this connection talks to.
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    /// The `GET /config/network` entry the endpoint was selected from, when a
    /// deployment was consulted.
    pub fn entry(&self) -> Option<&NetworkConfigEntry> {
        self.entry.as_ref()
    }

    /// Ask the node its `eth_chainId`, refuse a chain other than the expected
    /// one, and pin the answer as the chain id every tx is signed for.
    pub async fn verify_node(self) -> Result<Self, CmdError> {
        let node_chain = self
            .backend
            .chain_id()
            .await
            .map_err(|e| CmdError::Server(format!("eth_chainId from {}: {e}", self.rpc_url)))?;
        if let Some(expected) = self.expected {
            if node_chain != expected {
                return Err(chain_id_mismatch(format!(
                    "RPC node {} reports chain_id={node_chain} but this operation targets \
                     chain_id={expected}; refusing to sign or send",
                    self.rpc_url
                )));
            }
        }
        Ok(Self {
            chain_id: Some(node_chain),
            ..self
        })
    }

    /// The live path for a signing command: unlock the keystore and run the
    /// wallet-address preflight first (a bad wallet must fail before any node
    /// I/O), then — unless `ctx.dry_run`, which must open no RPC, or the node
    /// was already verified by [`Self::verify_node`] — check the node's
    /// chain id and pin it. Last, pick the receipt timeout (flag > config >
    /// chain-aware default).
    pub async fn into_session(
        self,
        ctx: &Ctx,
        wallet: &WalletOpts<'_>,
        purpose: &str,
    ) -> Result<TempoSession, CmdError> {
        self.into_session_unless(ctx, wallet, purpose, ctx.dry_run)
            .await
    }

    /// Like [`Self::into_session`], but the node is verified under
    /// `--dry-run` too. For flows that read the chain before signing: the
    /// reads must see a verified node, yet the wallet still comes first.
    pub async fn into_verified_session(
        self,
        ctx: &Ctx,
        wallet: &WalletOpts<'_>,
        purpose: &str,
    ) -> Result<TempoSession, CmdError> {
        self.into_session_unless(ctx, wallet, purpose, false).await
    }

    /// Wallet first, then verify the node unless `skip_verify` or it was
    /// already verified by [`Self::verify_node`].
    async fn into_session_unless(
        self,
        ctx: &Ctx,
        wallet: &WalletOpts<'_>,
        purpose: &str,
        skip_verify: bool,
    ) -> Result<TempoSession, CmdError> {
        let (signer, keystore_path) = load_wallet(ctx, wallet, purpose)?;
        let this = if skip_verify || self.chain_id.is_some() {
            self
        } else {
            self.verify_node().await?
        };
        Ok(this.session(ctx, wallet, signer, keystore_path))
    }

    fn session(
        self,
        ctx: &Ctx,
        wallet: &WalletOpts<'_>,
        signer: PrivateKeySigner,
        keystore_path: Option<PathBuf>,
    ) -> TempoSession {
        let default = self.chain_id.map_or_else(
            || default_receipt_timeout_for_network(ctx.environment.network()),
            default_receipt_timeout_for_chain_id,
        );
        TempoSession {
            receipt_timeout: resolve_duration(wallet.receipt_timeout, ctx.receipt_timeout, default),
            chain: self,
            signer,
            keystore_path,
        }
    }

    /// View call (`eth_call` with no explicit sender).
    pub async fn eth_call(&self, to: Address, calldata: &Bytes) -> Result<Vec<u8>, RpcError> {
        self.backend.eth_call(None, to, calldata).await
    }

    /// Raw JSON-RPC passthrough.
    pub async fn raw_call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.backend.raw_call(method, params).await
    }

    /// The node's chain id, once [`Self::verify_node`] has pinned it.
    pub fn chain_id(&self) -> Option<u64> {
        self.chain_id
    }
}

/// Deep session wrapping a signer, keystore file lock, and RPC transport.
#[derive(Clone, Debug)]
pub struct TempoSession {
    chain: ChainConnection,
    signer: PrivateKeySigner,
    keystore_path: Option<PathBuf>,
    receipt_timeout: Duration,
}

impl TempoSession {
    /// Create a new session with a live RPC client.
    ///
    /// Deprecated: the caller vouches for the node, nothing pins its chain id
    /// and the endpoint guards are skipped. Build sessions through
    /// [`ChainConnection::resolve`] + [`ChainConnection::into_session`].
    #[deprecated(note = "unpinned chain id; use ChainConnection::resolve + into_session")]
    pub fn new(
        signer: PrivateKeySigner,
        keystore_path: Option<PathBuf>,
        rpc: TempoRpcClient,
        receipt_timeout: Duration,
    ) -> Self {
        Self {
            chain: ChainConnection {
                backend: TempoRpcBackend::Client(rpc),
                rpc_url: String::new(),
                entry: None,
                expected: None,
                chain_id: None,
            },
            signer,
            keystore_path,
            receipt_timeout,
        }
    }

    /// Create a new session with an in-process mock adapter for testing.
    #[cfg(test)]
    pub fn from_mock(
        signer: PrivateKeySigner,
        keystore_path: Option<PathBuf>,
        mock: Arc<MockTempoRpc>,
        receipt_timeout: Duration,
    ) -> Self {
        Self {
            chain: ChainConnection {
                backend: TempoRpcBackend::Mock(mock),
                rpc_url: String::new(),
                entry: None,
                expected: None,
                chain_id: None,
            },
            signer,
            keystore_path,
            receipt_timeout,
        }
    }

    pub fn signer(&self) -> &PrivateKeySigner {
        &self.signer
    }

    pub fn address(&self) -> Address {
        self.signer.address()
    }

    /// View call on the session's verified chain (`eth_call`, no sender).
    pub async fn eth_call(&self, to: Address, calldata: &Bytes) -> Result<Vec<u8>, RpcError> {
        self.chain.eth_call(to, calldata).await
    }

    /// Raw JSON-RPC passthrough on the session's verified chain.
    pub async fn raw_call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.chain.raw_call(method, params).await
    }

    /// Read-only call impersonating an explicit sender (TIP-20 privacy compliance).
    pub async fn eth_call_from(
        &self,
        from: Address,
        to: Address,
        calldata: &Bytes,
    ) -> Result<Vec<u8>, CmdError> {
        self.chain
            .backend
            .eth_call(Some(from), to, calldata)
            .await
            .map_err(|e| CmdError::Server(format!("eth_call: {e}")))
    }

    /// Query ERC-20 balance for an account.
    pub async fn erc20_balance_of(
        &self,
        token: Address,
        account: Address,
    ) -> Result<U256, CmdError> {
        let calldata: Bytes = IERC20::balanceOfCall { account }.abi_encode().into();
        let raw = self.eth_call_from(account, token, &calldata).await?;
        decode_u256(&raw).map_err(CmdError::Decode)
    }

    /// Query ERC-20 allowance for (owner, spender).
    pub async fn erc20_allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> Result<U256, CmdError> {
        let calldata: Bytes = IERC20::allowanceCall { owner, spender }.abi_encode().into();
        let raw = self.eth_call_from(owner, token, &calldata).await?;
        decode_u256(&raw).map_err(CmdError::Decode)
    }

    /// Send a mutating transaction under the exclusive wallet lock without
    /// waiting for receipt, signed for the pinned chain id.
    pub async fn send_transaction(&self, to: Address, calldata: Bytes) -> Result<TxHash, CmdError> {
        self.send_transaction_for(self.chain.chain_id, to, calldata)
            .await
    }

    /// [`Self::send_transaction`] signed for `chain_id` when the caller
    /// checked something against it (the fee flow's allowlist).
    async fn send_transaction_for(
        &self,
        chain_id: Option<u64>,
        to: Address,
        calldata: Bytes,
    ) -> Result<TxHash, CmdError> {
        let _guard = self.acquire_lock()?;
        self.broadcast(to, calldata, chain_id)
            .await
            .map_err(|e| CmdError::Server(format!("tempo rpc: {e}")))
    }

    /// Send a mutating transaction under the exclusive wallet lock and wait for receipt.
    pub async fn send_transaction_and_wait(
        &self,
        to: Address,
        calldata: Bytes,
        action: &str,
        revert_suffix: Option<&str>,
    ) -> Result<TxHash, CmdError> {
        let _guard = self.acquire_lock()?;
        let (broadcast_msg, receipt_msg, tx_prefix) = if action.is_empty() {
            (
                "broadcast failed".to_string(),
                "receipt".to_string(),
                "tx".to_string(),
            )
        } else {
            (
                format!("{action} broadcast failed"),
                format!("{action} receipt"),
                format!("{action} tx"),
            )
        };
        let tx_hash = self
            .broadcast(to, calldata, self.chain.chain_id)
            .await
            .map_err(|e| CmdError::Server(format!("{broadcast_msg}: {e}")))?;
        let ok = self
            .wait_for_receipt(tx_hash)
            .await
            .map_err(|e| CmdError::Server(format!("{receipt_msg}: {e}")))?;
        if !ok {
            let suffix = revert_suffix.unwrap_or("");
            return Err(CmdError::Server(format!(
                "{tx_prefix} {tx_hash:#x} reverted on-chain{suffix}"
            )));
        }
        Ok(tx_hash)
    }

    /// Unified ERC-20 allowance check: checks current allowance and sends `approve` if needed.
    pub async fn ensure_allowance(
        &self,
        token: Address,
        spender: Address,
        required: U256,
    ) -> Result<Option<TxHash>, CmdError> {
        let current_allowance = self.erc20_allowance(token, self.address(), spender).await?;
        if current_allowance >= required {
            return Ok(None);
        }
        let approve_calldata: Bytes = IERC20::approveCall {
            spender,
            amount: required,
        }
        .abi_encode()
        .into();
        let approve_hash = self
            .send_transaction_and_wait(token, approve_calldata, "approve", None)
            .await?;
        let new_allowance = self.erc20_allowance(token, self.address(), spender).await?;
        if new_allowance < required {
            return Err(CmdError::Server(format!(
                "allowance still {new_allowance} after approve tx {approve_hash:#x} (needed {required})"
            )));
        }
        Ok(Some(approve_hash))
    }

    /// Sign and broadcast an ERC-20 submission-fee transfer under the per-wallet lock.
    ///
    /// Refuses to sign if the chain ID is not a known Tempo or local-dev network
    /// (unless `--allow-custom-endpoints` is active), or if the token is not on
    /// the PathUSD allowlist for that chain (AGENTS.md: a deliberate client-side
    /// guard against a compromised server steering the fee elsewhere).
    pub async fn send_fee_transfer(
        &self,
        ctx: &Ctx,
        token_address_str: &str,
        calldata: Bytes,
    ) -> Result<TxHash, CmdError> {
        let token_address: Address = token_address_str.parse().map_err(|e| {
            CmdError::Server(format!(
                "server returned invalid token_address `{token_address_str}`: {e}"
            ))
        })?;
        let chain_id = match self.chain.chain_id {
            Some(id) => id,
            None => self
                .chain
                .backend
                .chain_id()
                .await
                .map_err(|e| CmdError::Server(format!("tempo rpc eth_chainId: {e}")))?,
        };
        if !ctx.allow_custom_endpoints && !is_known_network(chain_id) {
            return Err(CmdError::Validation {
                code: "unknown_chain".into(),
                message: format!(
                    "RPC reports chain_id={chain_id} which is not a known Tempo or \
                     local-dev network; refusing to sign. Pass --allow-custom-endpoints \
                     to override."
                ),
            });
        }
        if !is_allowed_fee_token(chain_id, token_address_str) {
            return Err(CmdError::Validation {
                code: "fee_token_not_allowed".into(),
                message: format!(
                    "server returned token_address {token_address_str} which is not on the PathUSD \
                     allowlist for chain_id={chain_id}. Refusing to sign — this would \
                     redirect your fee transfer to an arbitrary ERC-20 contract."
                ),
            });
        }

        eprintln!(
            "taskfast: signing submission-fee transfer — token={} chain_id={} rpc={}",
            token_address_str, chain_id, self.chain.rpc_url
        );

        // Signed for the chain id the allowlist was checked against, even on
        // the unpinned (`TempoSession::new`) path.
        self.send_transaction_for(Some(chain_id), token_address, calldata)
            .await
    }

    /// Acquire the exclusive wallet lock for the keystore path, if configured.
    pub fn acquire_lock(&self) -> Result<Option<WalletGuard>, CmdError> {
        if let Some(ref path) = self.keystore_path {
            Ok(Some(wallet_lock::acquire(path)?))
        } else {
            Ok(None)
        }
    }

    async fn broadcast(
        &self,
        to: Address,
        calldata: Bytes,
        chain_id: Option<u64>,
    ) -> Result<TxHash, String> {
        self.chain
            .backend
            .broadcast(&self.signer, to, calldata, chain_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn wait_for_receipt(&self, tx_hash: TxHash) -> Result<bool, String> {
        self.chain
            .backend
            .wait_for_receipt(tx_hash, self.receipt_timeout)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Backend for RPC operations: live HTTP client or in-process mock.
#[derive(Clone, Debug)]
pub(crate) enum TempoRpcBackend {
    Client(TempoRpcClient),
    #[cfg(any(test, feature = "test-support"))]
    Mock(Arc<MockTempoRpc>),
}

impl TempoRpcBackend {
    /// A registered in-memory node (test support), else a live client with
    /// the client the URL's origin calls for.
    fn for_url(ctx: &Ctx, client: &TaskFastClient, rpc_url: &str) -> Self {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(mock) = registered_mock(rpc_url) {
            return Self::Mock(mock);
        }
        Self::Client(TempoRpcClient::new(
            rpc_http_client(ctx, client, rpc_url),
            rpc_url,
        ))
    }

    /// [`Self::for_url`] for an explicit `--rpc-url` with no deployment
    /// behind it. The proxy needs the `X-API-Key` header; a bare upstream
    /// node must not receive it.
    fn for_override(ctx: &Ctx, url: &str) -> Result<Self, CmdError> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(mock) = registered_mock(url) {
            return Ok(Self::Mock(mock));
        }
        let http = if is_proxy_rpc_url(url, ctx.base_url()) {
            ctx.client()?.http_client()
        } else {
            reqwest::Client::new()
        };
        Ok(Self::Client(TempoRpcClient::new(http, url)))
    }

    async fn chain_id(&self) -> Result<u64, RpcError> {
        match self {
            Self::Client(client) => client.chain_id().await,
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(mock) => {
                mock.touch();
                Ok(mock.chain_id.load(Ordering::SeqCst))
            }
        }
    }

    async fn eth_call(
        &self,
        from: Option<Address>,
        to: Address,
        calldata: &Bytes,
    ) -> Result<Vec<u8>, RpcError> {
        match self {
            Self::Client(client) => match from {
                Some(from) => client.eth_call_from(from, to, calldata).await,
                None => client.eth_call(to, calldata).await,
            },
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(mock) => mock.eth_call(to, calldata),
        }
    }

    async fn raw_call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match self {
            Self::Client(client) => client.raw_call(method, params).await,
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(mock) => mock.raw_call(method, params),
        }
    }

    async fn broadcast(
        &self,
        signer: &PrivateKeySigner,
        to: Address,
        calldata: Bytes,
        chain_id: Option<u64>,
    ) -> Result<TxHash, RpcError> {
        match self {
            Self::Client(client) => match chain_id {
                Some(chain_id) => {
                    sign_and_broadcast_tx_for_chain(client, signer, to, calldata, chain_id).await
                }
                None => sign_and_broadcast_tx(client, signer, to, calldata).await,
            },
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(mock) => mock.broadcast(signer, to, calldata, chain_id).await,
        }
    }

    async fn wait_for_receipt(&self, tx_hash: TxHash, timeout: Duration) -> Result<bool, RpcError> {
        match self {
            Self::Client(client) => {
                client
                    .wait_for_receipt(tx_hash, timeout, RECEIPT_POLL_INTERVAL)
                    .await
            }
            #[cfg(any(test, feature = "test-support"))]
            Self::Mock(mock) => {
                mock.touch();
                Ok(mock
                    .receipts
                    .lock()
                    .unwrap()
                    .get(&tx_hash)
                    .copied()
                    .unwrap_or(true))
            }
        }
    }
}

// ─── In-memory node for tests ─────────────────────────────────────────────

/// A scripted `eth_call` answer: `Ok(bytes)` or a revert message.
#[cfg(any(test, feature = "test-support"))]
type ScriptedCall = Result<Vec<u8>, String>;

/// In-memory node: balances, allowances, scripted `eth_call`s and raw
/// results, broadcast receipts. Register one for an RPC URL with
/// [`register_mock_rpc`] and every connection to that URL talks to it instead
/// of HTTP, so the real signing commands run end to end without a JSON-RPC
/// wiremock. Not built unless the `test-support` feature is on.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
#[derive(Debug)]
pub struct MockTempoRpc {
    /// Chain the node reports from `eth_chainId`.
    chain_id: AtomicU64,
    balances: Mutex<HashMap<(Address, Address), U256>>,
    allowances: Mutex<HashMap<(Address, Address, Address), U256>>,
    broadcast_delay: Mutex<Option<Duration>>,
    active_broadcasts: AtomicUsize,
    max_concurrent_broadcasts: AtomicUsize,
    /// Receipt status per tx hash; a hash with no entry succeeded.
    receipts: Mutex<HashMap<TxHash, bool>>,
    /// Every broadcast, in order: `(to, calldata)`.
    sent_transactions: Mutex<Vec<(Address, Bytes)>>,
    nonce: AtomicU64,
    /// `eth_call` results by calldata selector: `Ok(bytes)` or a revert message.
    calls: Mutex<HashMap<[u8; 4], ScriptedCall>>,
    /// Raw JSON-RPC results by method name.
    raw: Mutex<HashMap<String, Value>>,
    /// Selectors whose tx fails `eth_estimateGas` (the call reverts).
    estimate_reverts: Mutex<HashSet<[u8; 4]>>,
    /// Selectors whose tx is mined but reverts.
    receipt_reverts: Mutex<HashSet<[u8; 4]>>,
    /// Selectors whose raw tx the node refuses (`eth_sendRawTransaction`
    /// errors), with the error message.
    broadcast_rejects: Mutex<HashMap<[u8; 4], String>>,
    /// Every JSON-RPC request this node has handled.
    rpc_calls: AtomicUsize,
    /// Hash returned for each broadcast, aligned with `sent_transactions`.
    tx_hashes: Mutex<Vec<TxHash>>,
    /// Every raw JSON-RPC request so far: `(method, params)`.
    raw_calls: Mutex<Vec<(String, Value)>>,
    /// Chain id each broadcast was signed for, decoded from the signed tx
    /// (`None` = pre-EIP-155), aligned with `sent_transactions`.
    signed_chain_ids: Mutex<Vec<Option<u64>>>,
}

#[cfg(any(test, feature = "test-support"))]
impl Default for MockTempoRpc {
    fn default() -> Self {
        Self {
            chain_id: AtomicU64::new(42427),
            balances: Mutex::new(HashMap::new()),
            allowances: Mutex::new(HashMap::new()),
            broadcast_delay: Mutex::new(None),
            active_broadcasts: AtomicUsize::new(0),
            max_concurrent_broadcasts: AtomicUsize::new(0),
            receipts: Mutex::new(HashMap::new()),
            sent_transactions: Mutex::new(Vec::new()),
            nonce: AtomicU64::new(1),
            calls: Mutex::new(HashMap::new()),
            raw: Mutex::new(HashMap::new()),
            estimate_reverts: Mutex::new(HashSet::new()),
            receipt_reverts: Mutex::new(HashSet::new()),
            broadcast_rejects: Mutex::new(HashMap::new()),
            rpc_calls: AtomicUsize::new(0),
            tx_hashes: Mutex::new(Vec::new()),
            raw_calls: Mutex::new(Vec::new()),
            signed_chain_ids: Mutex::new(Vec::new()),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl MockTempoRpc {
    /// A node on chain 42427 with no state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Chain this node reports from `eth_chainId`.
    pub fn set_chain_id(&self, chain_id: u64) {
        self.chain_id.store(chain_id, Ordering::SeqCst);
    }

    /// `token.balanceOf(account)` answers `balance`.
    pub fn set_balance(&self, token: Address, account: Address, balance: U256) {
        self.balances
            .lock()
            .unwrap()
            .insert((token, account), balance);
    }

    /// `token.allowance(owner, spender)` answers `allowance`.
    pub fn set_allowance(&self, token: Address, owner: Address, spender: Address, allowance: U256) {
        self.allowances
            .lock()
            .unwrap()
            .insert((token, owner, spender), allowance);
    }

    /// Make every broadcast take `delay` (to exercise the wallet lock).
    pub fn set_broadcast_delay(&self, delay: Duration) {
        *self.broadcast_delay.lock().unwrap() = Some(delay);
    }

    /// Answer every `eth_call` whose calldata starts with `selector` (outside
    /// the token balance/allowance reads) with `result`.
    pub fn set_call_result(&self, selector: [u8; 4], result: Vec<u8>) {
        self.calls.lock().unwrap().insert(selector, Ok(result));
    }

    /// Make every `eth_call` starting with `selector` revert.
    pub fn set_call_revert(&self, selector: [u8; 4]) {
        self.calls
            .lock()
            .unwrap()
            .insert(selector, Err("execution reverted".to_string()));
    }

    /// Answer a raw JSON-RPC `method` with `result`.
    pub fn set_raw(&self, method: &str, result: Value) {
        self.raw.lock().unwrap().insert(method.to_string(), result);
    }

    /// Make the tx whose calldata starts with `selector` revert at gas
    /// estimation, so it is never broadcast.
    pub fn revert_estimate(&self, selector: [u8; 4]) {
        self.estimate_reverts.lock().unwrap().insert(selector);
    }

    /// Make the tx whose calldata starts with `selector` mine with status 0.
    pub fn revert_receipt(&self, selector: [u8; 4]) {
        self.receipt_reverts.lock().unwrap().insert(selector);
    }

    /// Make the node refuse the raw tx whose calldata starts with `selector`
    /// (`eth_sendRawTransaction` fails with `message`, e.g. insufficient
    /// funds): nothing is mined.
    pub fn reject_broadcast(&self, selector: [u8; 4], message: &str) {
        self.broadcast_rejects
            .lock()
            .unwrap()
            .insert(selector, message.to_string());
    }

    /// Every tx handed to the node so far, in order: `(to, calldata)`. A tx
    /// the node rejected (see [`Self::reject_broadcast`]) is still listed.
    pub fn sent_transactions(&self) -> Vec<(Address, Bytes)> {
        self.sent_transactions.lock().unwrap().clone()
    }

    /// Hash the node returned for each broadcast, aligned with
    /// [`Self::sent_transactions`].
    pub fn tx_hashes(&self) -> Vec<TxHash> {
        self.tx_hashes.lock().unwrap().clone()
    }

    /// Chain id each broadcast was signed for, decoded from the signed tx
    /// (so a pin that did not reach the signature shows up here), aligned
    /// with [`Self::sent_transactions`].
    pub fn broadcast_chain_ids(&self) -> Vec<Option<u64>> {
        self.signed_chain_ids.lock().unwrap().clone()
    }

    /// Every raw JSON-RPC request so far: `(method, params)`.
    pub fn raw_calls(&self) -> Vec<(String, Value)> {
        self.raw_calls.lock().unwrap().clone()
    }

    /// Number of JSON-RPC requests this node has handled.
    pub fn request_count(&self) -> usize {
        self.rpc_calls.load(Ordering::SeqCst)
    }

    fn touch(&self) {
        self.rpc_calls.fetch_add(1, Ordering::SeqCst);
    }

    fn eth_call(&self, to: Address, calldata: &[u8]) -> Result<Vec<u8>, RpcError> {
        self.touch();
        if let Ok(call) = IERC20::balanceOfCall::abi_decode(calldata) {
            let bal = self
                .balances
                .lock()
                .unwrap()
                .get(&(to, call.account))
                .copied()
                .unwrap_or(U256::ZERO);
            return Ok(bal.to_be_bytes::<32>().to_vec());
        }
        if let Ok(call) = IERC20::allowanceCall::abi_decode(calldata) {
            let allow = self
                .allowances
                .lock()
                .unwrap()
                .get(&(to, call.owner, call.spender))
                .copied()
                .unwrap_or(U256::ZERO);
            return Ok(allow.to_be_bytes::<32>().to_vec());
        }
        if let Some(result) = self.calls.lock().unwrap().get(&selector_of(calldata)) {
            return result
                .clone()
                .map_err(|message| RpcError::Rpc { code: 3, message });
        }
        Ok(vec![0u8; 32])
    }

    fn raw_call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.touch();
        self.raw_calls
            .lock()
            .unwrap()
            .push((method.to_string(), params));
        self.raw
            .lock()
            .unwrap()
            .get(method)
            .cloned()
            .ok_or_else(|| RpcError::Decode(format!("no scripted result for raw method {method}")))
    }

    /// Sign for real (so tests can check what the signature binds to), with
    /// the mock's nonce, 1 gwei and a fixed gas limit.
    async fn broadcast(
        &self,
        signer: &PrivateKeySigner,
        to: Address,
        calldata: Bytes,
        chain_id: Option<u64>,
    ) -> Result<TxHash, RpcError> {
        self.touch();
        let from = signer.address();
        let selector = selector_of(&calldata);
        if self.estimate_reverts.lock().unwrap().contains(&selector) {
            return Err(RpcError::Rpc {
                code: 3,
                message: "execution reverted".to_string(),
            });
        }
        let active = self.active_broadcasts.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrent_broadcasts
            .fetch_max(active, Ordering::SeqCst);

        let delay = *self.broadcast_delay.lock().unwrap();
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        self.sent_transactions
            .lock()
            .unwrap()
            .push((to, calldata.clone()));
        if let Some(message) = self.broadcast_rejects.lock().unwrap().get(&selector) {
            self.active_broadcasts.fetch_sub(1, Ordering::SeqCst);
            return Err(RpcError::Rpc {
                code: -32000,
                message: message.clone(),
            });
        }
        let approve = IERC20::approveCall::abi_decode(&calldata).ok();
        let nonce = self.nonce.fetch_add(1, Ordering::SeqCst);
        let node_chain = self.chain_id.load(Ordering::SeqCst);
        let (hash, raw) = sign_legacy_tx(
            signer,
            to,
            calldata,
            nonce,
            1_000_000_000,
            100_000,
            chain_id.unwrap_or(node_chain),
        )?;
        self.signed_chain_ids
            .lock()
            .unwrap()
            .push(signed_tx_chain_id(&raw)?);
        self.tx_hashes.lock().unwrap().push(hash);
        let reverted = self.receipt_reverts.lock().unwrap().contains(&selector);
        self.receipts.lock().unwrap().insert(hash, !reverted);
        // An approve only takes effect if it mined; a reverted one changes
        // nothing.
        if !reverted {
            if let Some(call) = approve {
                self.allowances
                    .lock()
                    .unwrap()
                    .insert((to, from, call.spender), call.amount);
            }
        }

        self.active_broadcasts.fetch_sub(1, Ordering::SeqCst);
        Ok(hash)
    }
}

#[cfg(any(test, feature = "test-support"))]
fn selector_of(calldata: &[u8]) -> [u8; 4] {
    let mut selector = [0u8; 4];
    let n = calldata.len().min(4);
    selector[..n].copy_from_slice(&calldata[..n]);
    selector
}

/// In-memory nodes by RPC URL. Tests register one under a unique URL and
/// point the command at it; nothing else ever reads this.
#[cfg(any(test, feature = "test-support"))]
static MOCK_RPCS: OnceLock<Mutex<HashMap<String, Arc<MockTempoRpc>>>> = OnceLock::new();

/// Serve every connection to `rpc_url` from `mock` instead of HTTP. Test
/// support: each test registers its node under its own URL (or under the proxy
/// URL its mocked `/config/network` advertises), so parallel tests never share
/// a node.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn register_mock_rpc(rpc_url: &str, mock: Arc<MockTempoRpc>) {
    MOCK_RPCS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(rpc_url.to_string(), mock);
}

#[cfg(any(test, feature = "test-support"))]
fn registered_mock(rpc_url: &str) -> Option<Arc<MockTempoRpc>> {
    MOCK_RPCS
        .get()
        .and_then(|m| m.lock().unwrap().get(rpc_url).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_signer() -> PrivateKeySigner {
        PrivateKeySigner::random()
    }

    #[tokio::test]
    async fn test_mock_balance_and_allowance() {
        let signer = test_signer();
        let mock = Arc::new(MockTempoRpc::new());
        let token = Address::repeat_byte(0x01);
        let spender = Address::repeat_byte(0x02);
        mock.set_balance(token, signer.address(), U256::from(5000));
        mock.set_allowance(token, signer.address(), spender, U256::from(1000));

        let session = TempoSession::from_mock(signer, None, mock, Duration::from_secs(10));

        let bal = session
            .erc20_balance_of(token, session.address())
            .await
            .unwrap();
        assert_eq!(bal, U256::from(5000));

        let allow = session
            .erc20_allowance(token, session.address(), spender)
            .await
            .unwrap();
        assert_eq!(allow, U256::from(1000));

        // When required <= current allowance, ensure_allowance returns Ok(None)
        let tx = session
            .ensure_allowance(token, spender, U256::from(800))
            .await
            .unwrap();
        assert!(tx.is_none());

        // When required > current allowance, ensure_allowance broadcasts approve and returns Ok(Some)
        let tx = session
            .ensure_allowance(token, spender, U256::from(2000))
            .await
            .unwrap();
        assert!(tx.is_some());

        let new_allow = session
            .erc20_allowance(token, session.address(), spender)
            .await
            .unwrap();
        assert_eq!(new_allow, U256::from(2000));
    }

    #[test]
    fn test_concurrency_serialization_under_wallet_lock() {
        let tmp = tempfile::TempDir::new().unwrap();
        let keystore_file = tmp.path().join("keystore.json");
        std::fs::write(&keystore_file, "{}").unwrap();

        let mock = Arc::new(MockTempoRpc::new());
        // Set broadcast delay so a concurrent broadcast must wait
        mock.set_broadcast_delay(Duration::from_millis(50));

        let mut threads = Vec::new();
        for _ in 0..4 {
            let signer = test_signer();
            let session = TempoSession::from_mock(
                signer,
                Some(keystore_file.clone()),
                mock.clone(),
                Duration::from_secs(5),
            );

            let handle = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    session
                        .send_transaction(Address::repeat_byte(0x05), Bytes::from_static(b"test"))
                        .await
                })
            });
            threads.push(handle);
        }

        for h in threads {
            let res = h.join().unwrap();
            assert!(res.is_ok());
        }

        // Under wallet_lock, max concurrency MUST be 1!
        assert_eq!(
            mock.max_concurrent_broadcasts.load(Ordering::SeqCst),
            1,
            "wallet_lock must serialize concurrent broadcasts so max concurrency is 1"
        );
        assert_eq!(mock.sent_transactions.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn mock_broadcast_honours_estimate_and_receipt_reverts() {
        let mock = Arc::new(MockTempoRpc::new());
        let session =
            TempoSession::from_mock(test_signer(), None, mock.clone(), Duration::from_secs(1));
        let selector = [0xaa, 0xbb, 0xcc, 0xdd];
        let calldata = Bytes::from(vec![0xaa, 0xbb, 0xcc, 0xdd, 0]);

        mock.revert_estimate(selector);
        let err = session
            .send_transaction(Address::ZERO, calldata.clone())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, CmdError::Server(m) if m.contains("execution reverted")),
            "{err:?}"
        );
        assert!(
            mock.sent_transactions().is_empty(),
            "a failed estimate is never broadcast"
        );

        mock.estimate_reverts.lock().unwrap().remove(&selector);
        mock.revert_receipt(selector);
        let err = session
            .send_transaction_and_wait(Address::ZERO, calldata, "open", Some(" — aborted"))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, CmdError::Server(m) if m.contains("reverted on-chain — aborted")),
            "{err:?}"
        );
        assert_eq!(mock.sent_transactions().len(), 1);
    }

    /// A node that refuses the raw tx: the broadcast is recorded, the RPC
    /// error surfaces, and the active-broadcast counter stays balanced.
    #[tokio::test]
    async fn mock_broadcast_rejection_surfaces_the_node_error() {
        let mock = Arc::new(MockTempoRpc::new());
        let session =
            TempoSession::from_mock(test_signer(), None, mock.clone(), Duration::from_secs(1));
        mock.reject_broadcast([0xaa, 0xbb, 0xcc, 0xdd], "insufficient funds");
        let err = session
            .send_transaction(
                Address::ZERO,
                Bytes::from_static(&[0xaa, 0xbb, 0xcc, 0xdd, 0]),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, CmdError::Server(m) if m.contains("insufficient funds")),
            "{err:?}"
        );
        assert_eq!(
            mock.sent_transactions().len(),
            1,
            "it was handed to the node"
        );
        assert_eq!(mock.active_broadcasts.load(Ordering::SeqCst), 0);
    }

    /// An `approve` the node refused, or that reverted, changed nothing.
    #[tokio::test]
    async fn mock_approve_updates_the_allowance_only_when_it_mined() {
        let mock = Arc::new(MockTempoRpc::new());
        let session =
            TempoSession::from_mock(test_signer(), None, mock.clone(), Duration::from_secs(1));
        let token = Address::repeat_byte(0x01);
        let spender = Address::repeat_byte(0x02);
        let approve: Bytes = IERC20::approveCall {
            spender,
            amount: U256::from(100),
        }
        .abi_encode()
        .into();
        let selector = selector_of(&approve);
        let allowance = || async {
            session
                .erc20_allowance(token, session.address(), spender)
                .await
                .unwrap()
        };

        mock.reject_broadcast(selector, "nonce too low");
        session
            .send_transaction(token, approve.clone())
            .await
            .unwrap_err();
        assert_eq!(allowance().await, U256::ZERO, "refused by the node");

        mock.broadcast_rejects.lock().unwrap().remove(&selector);
        mock.revert_receipt(selector);
        session
            .send_transaction_and_wait(token, approve, "approve", None)
            .await
            .unwrap_err();
        assert_eq!(allowance().await, U256::ZERO, "reverted on chain");

        mock.receipt_reverts.lock().unwrap().remove(&selector);
        session
            .send_transaction_and_wait(
                token,
                IERC20::approveCall {
                    spender,
                    amount: U256::from(100),
                }
                .abi_encode()
                .into(),
                "approve",
                None,
            )
            .await
            .unwrap();
        assert_eq!(allowance().await, U256::from(100));
    }

    #[test]
    fn is_proxy_rpc_url_matches_same_origin() {
        let base = "https://staging.api.taskfast.app";
        // Proxy mounted at /rpc/ — same origin, authenticated client.
        assert!(is_proxy_rpc_url(
            "https://staging.api.taskfast.app/rpc/testnet",
            base
        ));
        // Proxy mounted at /api/rpc/ (gh#79) — same origin, still ours.
        // The path differs but the host is the one the endpoint-guard
        // already approved, so the API key belongs here.
        assert!(is_proxy_rpc_url(
            "https://staging.api.taskfast.app/api/rpc/testnet",
            base
        ));
        // Trailing slash on base must not break the match.
        assert!(is_proxy_rpc_url(
            "https://staging.api.taskfast.app/rpc/mainnet",
            "https://staging.api.taskfast.app/"
        ));
        // Bare upstream Tempo gateway — different host, bare client is
        // correct (no TaskFast auth header to leak off-host).
        assert!(!is_proxy_rpc_url("https://rpc.moderato.tempo.xyz", base));
        // Different host, even one that pretends to be a proxy path,
        // must not pick up the authenticated client.
        assert!(!is_proxy_rpc_url(
            "https://attacker.example/rpc/testnet",
            base
        ));
        // Scheme downgrade is a *different* origin — never send the key
        // over plaintext, even to the same host.
        assert!(!is_proxy_rpc_url(
            "http://staging.api.taskfast.app/rpc/testnet",
            base
        ));
    }

    #[test]
    fn network_invariant_ok_when_single_advertised_matches() {
        let res = check_network_invariant(
            Environment::Prod,
            "https://api.taskfast.app",
            &["mainnet".into()],
            true,
        );
        assert!(res.is_ok(), "exact match passes strict mode: {res:?}");
    }

    #[test]
    fn network_invariant_strict_rejects_wrong_single_network() {
        let err = check_network_invariant(
            Environment::Prod,
            "https://api.taskfast.app",
            &["testnet".into()],
            true,
        )
        .expect_err("strict mode must reject wrong network");
        match err {
            CmdError::Server(msg) => {
                assert!(msg.contains("testnet"), "msg: {msg}");
                assert!(msg.contains("mainnet"), "msg: {msg}");
                assert!(msg.contains("issue #62"), "remediation hint: {msg}");
            }
            other => panic!("expected Server, got {other:?}"),
        }
    }

    #[test]
    fn network_invariant_strict_rejects_extra_advertised() {
        let err = check_network_invariant(
            Environment::Prod,
            "https://api.taskfast.app",
            &["mainnet".into(), "testnet".into()],
            true,
        )
        .expect_err("strict mode must reject len != 1");
        assert!(matches!(err, CmdError::Server(_)));
    }

    #[test]
    fn network_invariant_warn_passes_under_today_server_shape() {
        // Today's deployments advertise both networks. Warn-only mode
        // (default) keeps the CLI usable until issue #62 closes.
        let res = check_network_invariant(
            Environment::Staging,
            "https://staging.api.taskfast.app",
            &["mainnet".into(), "testnet".into()],
            false,
        );
        assert!(
            res.is_ok(),
            "non-strict mode must let today's multi-network deployments through: {res:?}"
        );
    }

    #[test]
    fn network_invariant_warn_passes_when_wrong_single_network() {
        // Even a wrong-single-network advertised should warn-only —
        // strict mode is opt-in.
        let res = check_network_invariant(
            Environment::Prod,
            "https://api.taskfast.app",
            &["testnet".into()],
            false,
        );
        assert!(res.is_ok(), "non-strict mode warns rather than errors");
    }

    /// End to end through the seam: a server-issued zone chain id selects the
    /// zone's own proxy entry, the node is verified against it, and the
    /// session signs for the verified chain.
    #[tokio::test]
    async fn resolve_by_chain_id_picks_the_zone_entry_and_pins_the_verified_chain() {
        let server = wiremock::MockServer::start().await;
        let uri = server.uri();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/config/network"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "networks": {
                    "testnet": { "chain_id": 42431, "rpc_url": format!("{uri}/rpc/testnet"), "wss_url": null, "explorer_url": null },
                    "tempo-zone-moderato": { "chain_id": 42432, "rpc_url": format!("{uri}/rpc/tempo-zone-moderato"), "wss_url": null, "explorer_url": null },
                }
            })))
            .mount(&server)
            .await;
        let ctx = Ctx {
            api_key: Some("k".into()),
            environment: Environment::Local,
            api_base: Some(uri.clone()),
            ..Default::default()
        };
        let client = ctx.client().unwrap();
        let zone_url = format!("{uri}/rpc/tempo-zone-moderato");
        let zone = Arc::new(MockTempoRpc::new());
        zone.set_chain_id(42432);
        register_mock_rpc(&zone_url, zone);

        let conn = ChainConnection::resolve(&ctx, &client, None, ChainTarget::ChainId(42432))
            .await
            .unwrap()
            .verify_node()
            .await
            .unwrap();
        assert_eq!(conn.rpc_url(), zone_url);
        assert_eq!(conn.chain_id(), Some(42432));
        assert_eq!(conn.entry().unwrap().chain_id, 42432);
    }

    #[tokio::test]
    async fn verify_node_refuses_a_node_on_another_chain() {
        let server = wiremock::MockServer::start().await;
        let uri = server.uri();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/config/network"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "networks": { "testnet": { "chain_id": 42431, "rpc_url": format!("{uri}/rpc/testnet"), "wss_url": null, "explorer_url": null } }
            })))
            .mount(&server)
            .await;
        let ctx = Ctx {
            api_key: Some("k".into()),
            environment: Environment::Local,
            api_base: Some(uri.clone()),
            ..Default::default()
        };
        let client = ctx.client().unwrap();
        let url = format!("{uri}/rpc/testnet");
        let node = Arc::new(MockTempoRpc::new());
        node.set_chain_id(1);
        register_mock_rpc(&url, node);

        let err =
            ChainConnection::resolve(&ctx, &client, None, ChainTarget::Network(Network::Testnet))
                .await
                .unwrap()
                .verify_node()
                .await
                .expect_err("wrong chain");
        assert!(
            matches!(&err, CmdError::Validation { code, .. } if code == "chain_id_mismatch"),
            "{err:?}"
        );
    }

    /// The live path over real HTTP (nothing registered): the proxy call
    /// carries the API key, `eth_chainId` is asked exactly once and the tx
    /// is signed for that chain, the receipt is polled; an off-host override
    /// never sees the key.
    #[tokio::test]
    async fn live_path_pins_the_verified_chain_and_routes_the_api_key() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};
        /// `(method, api key, first param)` of every JSON-RPC request.
        type Seen = Arc<Mutex<Vec<(String, Option<String>, String)>>>;
        async fn rpc_node(mount: &MockServer, route: &str, seen: Seen) {
            Mock::given(method("POST"))
                .and(path(route))
                .respond_with(move |req: &Request| {
                    let body: Value = serde_json::from_slice(&req.body).unwrap();
                    let name = body["method"].as_str().unwrap().to_string();
                    let key = req
                        .headers
                        .get("x-api-key")
                        .map(|v| v.to_str().unwrap().to_string());
                    let first = body["params"][0]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_default();
                    seen.lock().unwrap().push((name.clone(), key, first));
                    let result = match name.as_str() {
                        "eth_chainId" => serde_json::json!("0xa5bf"),
                        "eth_getTransactionCount" => serde_json::json!("0x0"),
                        "eth_gasPrice" => serde_json::json!("0x1"),
                        "eth_estimateGas" => serde_json::json!("0x10000"),
                        "eth_sendRawTransaction" => {
                            serde_json::json!(format!("{:#x}", TxHash::repeat_byte(0xab)))
                        }
                        "eth_getTransactionReceipt" => serde_json::json!({ "status": "0x1" }),
                        _ => serde_json::Value::Null,
                    };
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "jsonrpc": "2.0", "id": 1, "result": result,
                    }))
                })
                .mount(mount)
                .await;
        }

        let server = MockServer::start().await;
        let uri = server.uri();
        Mock::given(method("GET"))
            .and(path("/config/network"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "networks": { "testnet": { "chain_id": 42431, "rpc_url": format!("{uri}/rpc/testnet"), "wss_url": null, "explorer_url": null } }
            })))
            .mount(&server)
            .await;
        let proxy_seen: Seen = Arc::new(Mutex::new(Vec::new()));
        rpc_node(&server, "/rpc/testnet", proxy_seen.clone()).await;
        let ctx = Ctx {
            api_key: Some("k".into()),
            environment: Environment::Local,
            api_base: Some(uri.clone()),
            ..Default::default()
        };
        let client = ctx.client().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let keystore_path = tmp.path().join("wallet.json");
        taskfast_agent::keystore::save_signer(&test_signer(), &keystore_path, "pw").unwrap();
        let password = tmp.path().join("pw");
        std::fs::write(&password, b"pw").unwrap();
        let wallet = WalletOpts {
            keystore: Some(keystore_path.to_str().unwrap()),
            password_file: Some(&password),
            ..Default::default()
        };
        let session =
            ChainConnection::resolve(&ctx, &client, None, ChainTarget::Network(Network::Testnet))
                .await
                .unwrap()
                .into_session(&ctx, &wallet, "test")
                .await
                .unwrap();
        let tx = session
            .send_transaction_and_wait(
                Address::repeat_byte(0x05),
                Bytes::from_static(&[0xaa, 0xbb, 0xcc, 0xdd]),
                "test",
                None,
            )
            .await
            .unwrap();
        assert_eq!(tx, TxHash::repeat_byte(0xab));

        let seen = proxy_seen.lock().unwrap().clone();
        let methods: Vec<&str> = seen.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(
            methods,
            [
                "eth_chainId",
                "eth_getTransactionCount",
                "eth_gasPrice",
                "eth_estimateGas",
                "eth_sendRawTransaction",
                "eth_getTransactionReceipt",
            ],
            "verified once, then signed without asking again"
        );
        assert!(
            seen.iter().all(|r| r.1.as_deref() == Some("k")),
            "the deployment's proxy always carries the API key: {seen:?}"
        );
        let raw = hex::decode(seen[4].2.trim_start_matches("0x")).unwrap();
        assert_eq!(signed_tx_chain_id(&raw).unwrap(), Some(42431));

        // An off-host node with --allow-custom-endpoints: verified, but the
        // key never leaves the deployment.
        let other = MockServer::start().await;
        let other_seen: Seen = Arc::new(Mutex::new(Vec::new()));
        rpc_node(&other, "/", other_seen.clone()).await;
        let ctx = Ctx {
            allow_custom_endpoints: true,
            ..ctx
        };
        ChainConnection::open_override(&ctx, &other.uri())
            .unwrap()
            .verify_node()
            .await
            .unwrap();
        let seen = other_seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "eth_chainId");
        assert!(seen[0].1.is_none(), "off-host node saw the API key");
    }

    #[tokio::test]
    async fn resolve_refuses_a_proxy_url_off_the_deployment_host() {
        let server = wiremock::MockServer::start().await;
        let uri = server.uri();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/config/network"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "networks": { "testnet": { "chain_id": 42431, "rpc_url": "https://attacker.example/rpc/testnet", "wss_url": null, "explorer_url": null } }
            })))
            .mount(&server)
            .await;
        let ctx = Ctx {
            api_key: Some("k".into()),
            environment: Environment::Local,
            api_base: Some(uri.clone()),
            ..Default::default()
        };
        let client = ctx.client().unwrap();
        let err = ChainConnection::resolve(&ctx, &client, None, ChainTarget::Node)
            .await
            .expect_err("off-host proxy");
        assert!(
            matches!(&err, CmdError::Server(m) if m.contains("not on the same host")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn resolve_refuses_an_override_without_the_opt_in() {
        let server = wiremock::MockServer::start().await;
        let ctx = Ctx {
            api_key: Some("k".into()),
            environment: Environment::Local,
            api_base: Some(server.uri()),
            ..Default::default()
        };
        let client = ctx.client().unwrap();
        let err =
            ChainConnection::resolve(&ctx, &client, Some("http://rpc.invalid"), ChainTarget::Node)
                .await
                .expect_err("no opt-in");
        assert!(
            matches!(&err, CmdError::Usage(m) if m.contains("--allow-custom-endpoints")),
            "{err:?}"
        );
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "guard fires before any lookup"
        );
    }

    #[tokio::test]
    async fn config_fetch_keeps_rate_limit_errors_retryable() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/config/network"))
            .respond_with(
                wiremock::ResponseTemplate::new(429)
                    .insert_header("Retry-After", "7")
                    .set_body_json(serde_json::json!({ "error": "rate_limited" })),
            )
            .mount(&server)
            .await;
        let ctx = Ctx {
            api_key: Some("k".into()),
            environment: Environment::Local,
            api_base: Some(server.uri()),
            ..Default::default()
        };
        let client = ctx.client().unwrap();
        let err = ChainConnection::resolve(&ctx, &client, None, ChainTarget::Node)
            .await
            .expect_err("rate limited");
        assert!(
            matches!(&err, CmdError::RateLimited { .. } if err.retry_after().is_some()),
            "a throttled config fetch must stay retryable: {err:?}"
        );
    }
}
