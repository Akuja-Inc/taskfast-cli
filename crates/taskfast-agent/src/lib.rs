// SPDX-License-Identifier: MIT
//! Shared TaskFast agent logic for bootstrap, wallets and keystores, Tempo RPC,
//! on-chain signing, event pages, and webhook registration and verification.

// TODO: tighten doc coverage on public items + remove these allows.
// Tracked under the rust-best-practices follow-up rolled out alongside the
// taskfast-client 0.3 release.
#![allow(missing_docs)]

pub mod bootstrap;
pub mod chain;
pub mod events;
pub mod faucet;
pub mod keystore;
pub mod tempo_rpc;
pub mod wallet;
pub mod webhooks;
