// SPDX-License-Identifier: MIT
//! Agent onboarding: auth validation, headless creation, readiness gate.
//!
//! Mirrors the legacy `packages/agent/src/bootstrap.ts` shape (pre-Rust
//! rewrite) against the typed [`TaskFastClient`].
//! [`create_agent_headless`] is `scripts/init.sh` step 2 (BYOK-absent branch):
//! it returns the freshly-minted `api_key` (caller must persist; it never comes
//! back) and surfaces [`taskfast_client::Error`]. Auth validation and the
//! readiness gate are plain `client.call(|c| c.get_agent_profile())` and
//! `client.call(|c| c.get_agent_readiness())`. The create path adds one
//! invariant the generated client cannot enforce: the server *must* return
//! an `api_key`. If it doesn't, we fail loud rather than silently drop the
//! only credential the caller will ever see.

use taskfast_client::api::types::{AgentCreateRequest, AgentCreateResponse};
use taskfast_client::{Error, Result, TaskFastClient};

/// `POST /agents` — creates a headless agent on behalf of a human API key.
///
/// The returned `api_key` is the credential the agent will use from this
/// point on; the server will not reveal it again. We refuse to return a
/// response with a missing/empty key — that's a server contract violation
/// and silently proceeding would strand the caller without credentials.
pub async fn create_agent_headless(
    client: &TaskFastClient,
    body: &AgentCreateRequest,
) -> Result<AgentCreateResponse> {
    let resp = client.call(|c| c.register_agent(body)).await?;
    match resp.api_key.as_deref() {
        Some(k) if !k.is_empty() => Ok(resp),
        _ => Err(Error::Server(
            "createAgentHeadless: response missing api_key — cannot persist credentials".into(),
        )),
    }
}

pub use crate::wallet::WalletRegistration;

/// `POST /agents/me/wallet` — bind a Tempo wallet address to the agent.
///
/// Deprecated: use [`crate::wallet::register_wallet`] directly.
#[deprecated(note = "use taskfast_agent::wallet::register_wallet")]
pub async fn register_wallet(
    client: &TaskFastClient,
    body: &taskfast_client::api::types::WalletSetupRequest,
) -> Result<WalletRegistration> {
    crate::wallet::register_wallet(client, body.tempo_wallet_address.as_str()).await
}
