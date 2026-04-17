//! Stellar stub — architecture-readiness only (beads am-6v7b.4). No body in v1.

use crate::chain::Chain;

#[derive(Debug, Clone, Copy, Default)]
pub struct Stellar;

#[derive(Debug, Clone, Copy, Default)]
pub struct Network;

impl Chain for Stellar {
    type Address = ();
    type Signature = ();
    type TxHash = ();
    type EscrowRef = ();
    type Network = Network;

    fn id() -> &'static str {
        "stellar"
    }
    fn network(&self) -> &Self::Network {
        &Network
    }
}
