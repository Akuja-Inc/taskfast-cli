//! Chain abstraction for the TaskFast Rust SDK.
//!
//! v1 ships `tempo` only — other features compile to empty stub modules so the
//! feature matrix (and downstream `cargo check`) is exercised in CI without
//! implementing a chain. Mirrors the Elixir `TaskFast.Chain` behaviour split
//! (beads am-6v7b.1 / am-6v7b.2).

#![allow(missing_docs)]

pub mod chain;
pub mod any;

#[cfg(feature = "tempo")]
pub mod tempo;

#[cfg(feature = "polygon")]
pub mod polygon;

#[cfg(feature = "avalanche")]
pub mod avalanche;

#[cfg(feature = "solana")]
pub mod solana;

#[cfg(feature = "near")]
pub mod near;

#[cfg(feature = "stellar")]
pub mod stellar;

pub use any::AnyChain;
pub use chain::Chain;
