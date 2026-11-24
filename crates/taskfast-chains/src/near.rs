// SPDX-License-Identifier: MIT
//! Near stub — architecture-readiness only. No body in v1.

use crate::chain::Chain;

#[derive(Debug, Clone, Copy, Default)]
pub struct Near;

#[derive(Debug, Clone, Copy, Default)]
pub struct Network;

impl Chain for Near {
    type Address = ();
    type Signature = ();
    type TxHash = ();
    type EscrowRef = ();
    type Network = Network;

    fn id() -> &'static str {
        "near"
    }
    fn network(&self) -> &Self::Network {
        &Network
    }
}
