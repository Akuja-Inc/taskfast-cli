//! `trait Chain` — identity + associated-type seats for per-chain primitives.
//!
//! Deliberately identity-only in v1. Signing / approval-codec surfaces stay as
//! free fns inside each chain module (see `tempo::sign_distribution`) so we
//! don't bake a premature uniform API before non-Tempo impls exist. Mirrors
//! the Elixir split: `TaskFast.Chain` is identity + RPC, `ApprovalCodec` is a
//! separate behaviour.

pub trait Chain {
    type Address;
    type Signature;
    type TxHash;
    type EscrowRef;
    type Network;

    fn id() -> &'static str;
    fn network(&self) -> &Self::Network;
}
