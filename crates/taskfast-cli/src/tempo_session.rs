// SPDX-License-Identifier: MIT
//! Deep Tempo transaction and wallet signing session module.
//!
//! Encapsulates keystore handling, address validation, RPC client binding,
//! ERC-20 allowances/balances, and automatically serializes every mutating
//! transaction broadcast under the exclusive [`wallet_lock`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy_primitives::{Address, Bytes, TxHash, U256};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;

use taskfast_agent::chain::IERC20;
use taskfast_agent::tempo_rpc::{sign_and_broadcast_tx, TempoRpcClient};
use taskfast_chains::tempo::TEMPO_MAINNET_CHAIN_ID;

use crate::cmd::{
    is_proxy_rpc_url, network_policy_for_chain_id, resolve_duration, validate_override_rpc_url,
    CmdError, Ctx,
};
use crate::wallet_lock::{self, WalletGuard};
use crate::Network;

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

/// In-process mock RPC adapter for hermetic tests without WireMock scaffolding.
#[derive(Debug)]
pub struct MockTempoRpc {
    pub chain_id: AtomicU64,
    pub balances: Mutex<HashMap<(Address, Address), U256>>,
    pub allowances: Mutex<HashMap<(Address, Address, Address), U256>>,
    pub broadcast_delay: Mutex<Option<Duration>>,
    pub active_broadcasts: std::sync::atomic::AtomicUsize,
    pub max_concurrent_broadcasts: std::sync::atomic::AtomicUsize,
    pub receipts: Mutex<HashMap<TxHash, bool>>,
    pub default_receipt_status: AtomicBool,
    pub sent_transactions: Mutex<Vec<(Address, Bytes)>>,
    pub calls: Mutex<HashMap<(Address, Bytes), Vec<u8>>>,
    pub raw_responses: Mutex<HashMap<String, serde_json::Value>>,
}

impl Default for MockTempoRpc {
    fn default() -> Self {
        Self {
            chain_id: AtomicU64::new(42427),
            balances: Mutex::new(HashMap::new()),
            allowances: Mutex::new(HashMap::new()),
            broadcast_delay: Mutex::new(None),
            active_broadcasts: std::sync::atomic::AtomicUsize::new(0),
            max_concurrent_broadcasts: std::sync::atomic::AtomicUsize::new(0),
            receipts: Mutex::new(HashMap::new()),
            default_receipt_status: AtomicBool::new(true),
            sent_transactions: Mutex::new(Vec::new()),
            calls: Mutex::new(HashMap::new()),
            raw_responses: Mutex::new(HashMap::new()),
        }
    }
}

impl MockTempoRpc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_chain_id(chain_id: u64) -> Self {
        Self {
            chain_id: AtomicU64::new(chain_id),
            ..Default::default()
        }
    }

    pub fn set_balance(&self, token: Address, account: Address, balance: U256) {
        self.balances
            .lock()
            .unwrap()
            .insert((token, account), balance);
    }

    pub fn set_allowance(&self, token: Address, owner: Address, spender: Address, allowance: U256) {
        self.allowances
            .lock()
            .unwrap()
            .insert((token, owner, spender), allowance);
    }

    pub fn set_broadcast_delay(&self, delay: Duration) {
        *self.broadcast_delay.lock().unwrap() = Some(delay);
    }
}

/// Backend variant for RPC operations: live HTTP client or in-process mock.
#[derive(Clone, Debug)]
pub enum TempoRpcBackend {
    Client(TempoRpcClient),
    Mock(Arc<MockTempoRpc>),
}

/// Deep session wrapping a signer, keystore file lock, and RPC transport.
#[derive(Clone, Debug)]
pub struct TempoSession {
    signer: PrivateKeySigner,
    keystore_path: Option<PathBuf>,
    backend: TempoRpcBackend,
    chain_id: u64,
    receipt_timeout: Duration,
    poll_interval: Duration,
}

impl TempoSession {
    /// Create a new session with a live RPC client.
    pub fn new(
        signer: PrivateKeySigner,
        keystore_path: Option<PathBuf>,
        rpc: TempoRpcClient,
        chain_id: u64,
        receipt_timeout: Duration,
        poll_interval: Duration,
    ) -> Self {
        Self {
            signer,
            keystore_path,
            backend: TempoRpcBackend::Client(rpc),
            chain_id,
            receipt_timeout,
            poll_interval,
        }
    }

    /// Create a new session with an in-process mock adapter.
    pub fn from_mock(
        signer: PrivateKeySigner,
        keystore_path: Option<PathBuf>,
        mock: Arc<MockTempoRpc>,
        chain_id: u64,
        receipt_timeout: Duration,
        poll_interval: Duration,
    ) -> Self {
        Self {
            signer,
            keystore_path,
            backend: TempoRpcBackend::Mock(mock),
            chain_id,
            receipt_timeout,
            poll_interval,
        }
    }

    /// Resolve RPC endpoint and create a session against the live network.
    pub async fn resolve_and_connect(
        signer: PrivateKeySigner,
        keystore_path: Option<PathBuf>,
        ctx: &Ctx,
        override_url: Option<&str>,
        target_chain_id: Option<i64>,
        cli_receipt_timeout: Option<Duration>,
    ) -> Result<Self, CmdError> {
        let (rpc_url, http) = resolve_rpc_endpoint(ctx, override_url, target_chain_id).await?;
        let rpc = TempoRpcClient::new(http, rpc_url);
        let chain_id = rpc
            .chain_id()
            .await
            .map_err(|e| CmdError::Server(format!("tempo rpc eth_chainId: {e}")))?;
        let default_timeout = default_receipt_timeout_for_chain_id(chain_id);
        let receipt_timeout =
            resolve_duration(cli_receipt_timeout, ctx.receipt_timeout, default_timeout);
        Ok(Self::new(
            signer,
            keystore_path,
            rpc,
            chain_id,
            receipt_timeout,
            RECEIPT_POLL_INTERVAL,
        ))
    }

    pub fn signer(&self) -> &PrivateKeySigner {
        &self.signer
    }

    pub fn address(&self) -> Address {
        self.signer.address()
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub fn keystore_path(&self) -> Option<&Path> {
        self.keystore_path.as_deref()
    }

    pub fn receipt_timeout(&self) -> Duration {
        self.receipt_timeout
    }

    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    pub fn rpc_client(&self) -> Option<&TempoRpcClient> {
        match &self.backend {
            TempoRpcBackend::Client(c) => Some(c),
            TempoRpcBackend::Mock(_) => None,
        }
    }

    pub fn mock_adapter(&self) -> Option<&Arc<MockTempoRpc>> {
        match &self.backend {
            TempoRpcBackend::Mock(m) => Some(m),
            TempoRpcBackend::Client(_) => None,
        }
    }

    /// Acquire the exclusive wallet lock for the keystore path, if configured.
    pub fn acquire_lock(&self) -> Result<Option<WalletGuard>, CmdError> {
        if let Some(ref path) = self.keystore_path {
            Ok(Some(wallet_lock::acquire(path)?))
        } else {
            Ok(None)
        }
    }

    /// Read-only call without explicit sender.
    pub async fn eth_call(&self, to: Address, calldata: &Bytes) -> Result<Vec<u8>, CmdError> {
        self.eth_call_from(self.address(), to, calldata).await
    }

    /// Read-only call impersonating an explicit sender (TIP-20 privacy compliance).
    pub async fn eth_call_from(
        &self,
        from: Address,
        to: Address,
        calldata: &Bytes,
    ) -> Result<Vec<u8>, CmdError> {
        match &self.backend {
            TempoRpcBackend::Client(client) => client
                .eth_call_from(from, to, calldata)
                .await
                .map_err(|e| CmdError::Server(format!("eth_call: {e}"))),
            TempoRpcBackend::Mock(mock) => {
                if let Ok(call) = IERC20::balanceOfCall::abi_decode(calldata) {
                    let bal = mock
                        .balances
                        .lock()
                        .unwrap()
                        .get(&(to, call.account))
                        .copied()
                        .unwrap_or(U256::ZERO);
                    return Ok(bal.to_be_bytes::<32>().to_vec());
                }
                if let Ok(call) = IERC20::allowanceCall::abi_decode(calldata) {
                    let allow = mock
                        .allowances
                        .lock()
                        .unwrap()
                        .get(&(to, call.owner, call.spender))
                        .copied()
                        .unwrap_or(U256::ZERO);
                    return Ok(allow.to_be_bytes::<32>().to_vec());
                }
                if let Some(res) = mock.calls.lock().unwrap().get(&(to, calldata.clone())) {
                    return Ok(res.clone());
                }
                Ok(vec![0u8; 32])
            }
        }
    }

    /// Raw JSON-RPC method invocation.
    pub async fn raw_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CmdError> {
        match &self.backend {
            TempoRpcBackend::Client(client) => client
                .raw_call(method, params)
                .await
                .map_err(|e| CmdError::Server(format!("{method}: {e}"))),
            TempoRpcBackend::Mock(mock) => {
                if let Some(val) = mock.raw_responses.lock().unwrap().get(method) {
                    return Ok(val.clone());
                }
                Ok(serde_json::json!("0x0"))
            }
        }
    }

    /// Query ERC-20 balance for an account.
    pub async fn erc20_balance_of(
        &self,
        token: Address,
        account: Address,
    ) -> Result<U256, CmdError> {
        let calldata: Bytes = IERC20::balanceOfCall { account }.abi_encode().into();
        let raw = self.eth_call_from(account, token, &calldata).await?;
        decode_u256(&raw, "balanceOf")
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
        decode_u256(&raw, "allowance")
    }

    /// Send a mutating transaction under the exclusive wallet lock without waiting for receipt.
    pub async fn send_transaction(&self, to: Address, calldata: Bytes) -> Result<TxHash, CmdError> {
        let _guard = self.acquire_lock()?;
        self.broadcast_internal(to, calldata).await
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
            .broadcast_internal(to, calldata)
            .await
            .map_err(|e| CmdError::Server(format!("{broadcast_msg}: {e}")))?;
        let ok = self
            .wait_for_receipt_internal(tx_hash)
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

    async fn broadcast_internal(&self, to: Address, calldata: Bytes) -> Result<TxHash, CmdError> {
        match &self.backend {
            TempoRpcBackend::Client(client) => {
                sign_and_broadcast_tx(client, &self.signer, to, calldata)
                    .await
                    .map_err(|e| CmdError::Server(format!("{e}")))
            }
            TempoRpcBackend::Mock(mock) => {
                let active = mock.active_broadcasts.fetch_add(1, Ordering::SeqCst) + 1;
                mock.max_concurrent_broadcasts
                    .fetch_max(active, Ordering::SeqCst);

                let delay = *mock.broadcast_delay.lock().unwrap();
                if let Some(d) = delay {
                    tokio::time::sleep(d).await;
                }
                if let Ok(call) = IERC20::approveCall::abi_decode(&calldata) {
                    mock.allowances
                        .lock()
                        .unwrap()
                        .insert((to, self.address(), call.spender), call.amount);
                }
                mock.sent_transactions
                    .lock()
                    .unwrap()
                    .push((to, calldata.clone()));
                let hash = alloy_primitives::keccak256(&calldata);

                mock.active_broadcasts.fetch_sub(1, Ordering::SeqCst);
                Ok(TxHash::from(hash))
            }
        }
    }

    async fn wait_for_receipt_internal(&self, tx_hash: TxHash) -> Result<bool, CmdError> {
        match &self.backend {
            TempoRpcBackend::Client(client) => client
                .wait_for_receipt(tx_hash, self.receipt_timeout, self.poll_interval)
                .await
                .map_err(|e| CmdError::Server(format!("{e}"))),
            TempoRpcBackend::Mock(mock) => {
                let ok = mock
                    .receipts
                    .lock()
                    .unwrap()
                    .get(&tx_hash)
                    .copied()
                    .unwrap_or_else(|| mock.default_receipt_status.load(Ordering::Relaxed));
                Ok(ok)
            }
        }
    }
}

/// Helper function to decode standard 32-byte BE encoded U256 return values.
fn decode_u256(bytes: &[u8], label: &str) -> Result<U256, CmdError> {
    if bytes.len() < 32 {
        return Err(CmdError::Decode(format!(
            "{label} returned {} bytes, expected >=32",
            bytes.len()
        )));
    }
    Ok(U256::from_be_slice(&bytes[..32]))
}

/// Resolve the RPC URL and reqwest client from either an override or `/config/network`.
pub async fn resolve_rpc_endpoint(
    ctx: &Ctx,
    override_url: Option<&str>,
    target_chain_id: Option<i64>,
) -> Result<(String, reqwest::Client), CmdError> {
    if let Some(url) = override_url {
        let network = match target_chain_id {
            Some(id) => network_policy_for_chain_id(id as u64),
            None => ctx.environment.network(),
        };
        validate_override_rpc_url(url, network, ctx.allow_custom_endpoints)?;
        let http = if is_proxy_rpc_url(url, ctx.base_url()) {
            ctx.client()?.http_client()
        } else {
            reqwest::Client::new()
        };
        return Ok((url.to_string(), http));
    }

    let client = ctx.client()?;
    let cfg = client.fetch_network_config().await.map_err(|e| match e {
        taskfast_client::Error::Auth(_) | taskfast_client::Error::Validation { .. } => {
            CmdError::Server(format!("fetch network config from {}: {e}", ctx.base_url()))
        }
        other => CmdError::from(other),
    })?;

    let rpc_url = if let Some(target_id) = target_chain_id {
        let (_name, entry) = cfg.entry_by_chain_id(target_id).map_err(|e| {
            CmdError::Server(format!(
                "deployment at {} does not advertise chain_id={target_id}: {e}",
                ctx.base_url()
            ))
        })?;
        if !is_proxy_rpc_url(&entry.rpc_url, ctx.base_url()) {
            return Err(CmdError::Server(format!(
                "deployment at {} returned rpc_url {:?} for chain_id {target_id}, which is not on the same host. \
                 Refusing to route RPC traffic off-host.",
                ctx.base_url(),
                entry.rpc_url,
            )));
        }
        entry.rpc_url.clone()
    } else {
        let name = ctx.environment.network().as_str();
        let entry = cfg.entry(name).map_err(|e| {
            CmdError::Server(format!(
                "deployment at {} does not advertise network `{name}`: {e}",
                ctx.base_url()
            ))
        })?;
        if !is_proxy_rpc_url(&entry.rpc_url, ctx.base_url()) {
            return Err(CmdError::Server(format!(
                "deployment at {} returned rpc_url {:?} for network `{name}`, which is not on \
                 the same host. Refusing to route RPC traffic off-host.",
                ctx.base_url(),
                entry.rpc_url,
            )));
        }
        entry.rpc_url.clone()
    };

    Ok((rpc_url, client.http_client()))
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

        let session = TempoSession::from_mock(
            signer,
            None,
            mock,
            42427,
            Duration::from_secs(10),
            Duration::from_millis(50),
        );

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
                42427,
                Duration::from_secs(5),
                Duration::from_millis(10),
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
}
