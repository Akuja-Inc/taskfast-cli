// SPDX-License-Identifier: MIT
//! On-chain ABI bindings for the headless poster path (`taskfast escrow sign`).
//!
//! Mirrors the JS wagmi bindings in `assets/js/escrow_sign.js` on the platform
//! side. The two must stay byte-compatible: the Rust CLI pre-computes an
//! `escrowId` that Solidity's `TaskEscrow.computeEscrowId` re-derives post
//! transfer — any drift and the signature the poster pre-signed becomes
//! unusable.
//!
//! # Fee-on-transfer caveat
//!
//! `TaskEscrow.open` computes its on-chain `escrowId` using `actualDeposit`
//! (the post-transfer balance delta, not the supplied `deposit`). For a
//! standard ERC-20 the two are equal, so the prediction matches. A
//! fee-on-transfer token would diverge — callers rely on the platform's
//! `allowedTokens` allowlist (fee-on-transfer excluded) to keep this safe.
//!
//! # Why a separate module (not folded into `signing.rs`)
//!
//! `signing.rs` is the EIP-712 typed-data surface (hash + sign primitives).
//! These bindings are non-712: plain ABI `sol!` contract definitions used to
//! encode calldata for `approve`/`open`. Keeping them separate keeps the
//! signing module's scope tight and avoids pulling ERC-20 view calls into a
//! crate section that shouldn't care about them.

use alloy_primitives::{keccak256, Address, B256, U256};
#[cfg(test)]
use alloy_sol_types::SolCall;
use alloy_sol_types::{sol, SolValue};

sol! {
    /// TaskEscrow contract surface used by the poster flow. Only the mutating
    /// functions needed for `open` / `openWithMemo` are bound — full surface
    /// lives in the platform's Solidity source.
    #[allow(missing_docs)]
    contract TaskEscrow {
        function open(
            address token,
            uint256 deposit,
            address worker,
            uint256 platformFeeAmount,
            address platform,
            address arbitrator,
            bytes32 salt
        ) external returns (bytes32);

        function openWithMemo(
            address token,
            uint256 deposit,
            address worker,
            uint256 platformFeeAmount,
            address platform,
            address arbitrator,
            bytes32 salt,
            bytes32 memoHash
        ) external returns (bytes32);
    }

    /// Minimal ERC-20 surface needed to pre-flight the escrow deposit. Only
    /// `approve`, `allowance`, and `balanceOf` are bound; `transfer` is
    /// handled through the existing ERC-20 transfer helper in `tempo_rpc`.
    #[allow(missing_docs)]
    contract IERC20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
    }

    /// TaskBond contract surface used by the auction-task operator bond flow
    /// (`taskfast bond post`, gh#95). Only `post` is bound. The server verifies
    /// the resulting `BondPosted` event by matching contract/token/`taskRef`/
    /// amount — it does *not* pin `salt`, so any salt is accepted.
    #[allow(missing_docs)]
    contract TaskBond {
        function post(
            address token,
            uint256 amount,
            bytes32 taskRef,
            bytes32 salt
        ) external;
    }
}

/// Derive the `bytes32 taskRef` that `TaskBond.post` binds for a given task.
///
/// The server verifier expects `taskRef` = 16 zero bytes concatenated with the
/// task UUID's 16 raw bytes (gh#95). Kept next to [`compute_escrow_id`] because
/// it is the same class of "must byte-match the on-chain/server derivation"
/// helper: drift here silently posts a bond the verifier will never accept.
pub fn compute_task_ref(task_id: uuid::Uuid) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[16..].copy_from_slice(task_id.as_bytes());
    B256::from(bytes)
}

/// Inputs to [`compute_escrow_id`] — one named field per preimage element.
///
/// Five fields are `Address` (`poster`, `worker`, `token`, `platform`,
/// `arbitrator`). A positional argument list would let a caller silently
/// transpose two of them and pre-sign a `DistributionApproval` against the
/// wrong escrow id; named fields make the call site self-checking.
pub struct EscrowIdParams {
    /// Task poster funding the escrow.
    pub poster: Address,
    /// Worker who will be paid out.
    pub worker: Address,
    /// ERC-20 deposit token.
    pub token: Address,
    /// Deposit amount (pre fee-on-transfer; see module-level caveat).
    pub deposit: U256,
    /// Platform fee taken from the deposit.
    pub platform_fee_amount: U256,
    /// Platform wallet receiving the fee.
    pub platform: Address,
    /// Pool arbitrator bound to the escrow (v2; must be non-zero on-chain).
    pub arbitrator: Address,
    /// Random 32-byte salt.
    pub salt: B256,
}

/// Predict the `escrowId` that `ArbitratedEscrow.open` will assign to a new escrow.
///
/// Computed as `keccak256(abi.encode(poster, worker, token, deposit, fee,
/// platform, arbitrator, salt))` — must byte-match the contract's derivation in
/// `ArbitratedEscrow.computeEscrowId` (`contracts/src/ArbitratedEscrow.sol`).
/// The `arbitrator` is part of the preimage in canonical v2, so omitting it
/// yields the wrong id (and a no-arbitrator `open` reverts on the v2 contract).
/// Exposed so callers can pre-sign the EIP-712 `DistributionApproval` *before*
/// broadcasting the `open` tx.
pub fn compute_escrow_id(p: &EscrowIdParams) -> B256 {
    let encoded = (
        p.poster,
        p.worker,
        p.token,
        p.deposit,
        p.platform_fee_amount,
        p.platform,
        p.arbitrator,
        p.salt,
    )
        .abi_encode();
    keccak256(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known-answer test pinning the `escrowId` preimage byte-layout.
    ///
    /// The expected digest is a frozen constant, NOT re-derived in-test — any
    /// change to the field order / types / count inside [`compute_escrow_id`]
    /// (e.g. moving `arbitrator`) flips it and fails CI. Regenerate ONLY from
    /// the canonical contract/JS derivation (`ArbitratedEscrow.computeEscrowId`
    /// / `assets/js/escrow_sign.js`) when the on-chain preimage intentionally
    /// changes; pasting fresh Rust output back in would silently re-tautologize
    /// the test and lose the cross-impl guarantee.
    #[test]
    fn compute_escrow_id_matches_known_vector() {
        let mut salt_bytes = [0u8; 32];
        salt_bytes[31] = 0x42;
        let params = EscrowIdParams {
            poster: "0x0000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
            worker: "0x0000000000000000000000000000000000000002"
                .parse()
                .unwrap(),
            token: "0x0000000000000000000000000000000000000003"
                .parse()
                .unwrap(),
            deposit: U256::from(1_000_000_000u64),
            platform_fee_amount: U256::from(50_000_000u64),
            platform: "0x0000000000000000000000000000000000000004"
                .parse()
                .unwrap(),
            arbitrator: "0x0000000000000000000000000000000000000005"
                .parse()
                .unwrap(),
            salt: B256::from(salt_bytes),
        };

        // Frozen digest for the 8-field preimage in canonical order
        // (poster, worker, token, deposit, fee, platform, arbitrator, salt).
        let expected: B256 = "0x1fada0b25ef1b0a435fd6b8ef78d4a068b13e77c95bb29635397ab0106835363"
            .parse()
            .expect("pinned escrow-id vector");
        assert_eq!(compute_escrow_id(&params), expected);
    }

    #[test]
    fn compute_escrow_id_is_salt_sensitive() {
        let base = |salt: B256| EscrowIdParams {
            poster: Address::ZERO,
            worker: Address::ZERO,
            token: Address::ZERO,
            deposit: U256::from(1u64),
            platform_fee_amount: U256::from(0u64),
            platform: Address::ZERO,
            arbitrator: Address::ZERO,
            salt,
        };

        let mut salt_a = [0u8; 32];
        salt_a[31] = 0x01;
        let mut salt_b = [0u8; 32];
        salt_b[31] = 0x02;

        let a = compute_escrow_id(&base(B256::from(salt_a)));
        let b = compute_escrow_id(&base(B256::from(salt_b)));
        assert_ne!(a, b, "distinct salts must produce distinct escrow ids");
    }

    #[test]
    fn approve_calldata_has_expected_selector() {
        // ERC-20 `approve(address,uint256)` selector is 0x095ea7b3.
        let call = IERC20::approveCall {
            spender: Address::ZERO,
            amount: U256::from(1u64),
        };
        let data = call.abi_encode();
        assert_eq!(&data[0..4], &[0x09, 0x5e, 0xa7, 0xb3]);
    }

    #[test]
    fn post_bond_calldata_has_expected_selector() {
        // `TaskBond.post(address,uint256,bytes32,bytes32)` — the macro-derived
        // selector must equal keccak(canonical signature)[..4]. Arg-order/type
        // drift (e.g. swapping taskRef/salt or widening amount) fails this.
        let call = TaskBond::postCall {
            token: Address::ZERO,
            amount: U256::from(1u64),
            taskRef: B256::ZERO,
            salt: B256::ZERO,
        };
        let data = call.abi_encode();
        let expected = &keccak256("post(address,uint256,bytes32,bytes32)")[..4];
        assert_eq!(&data[0..4], expected);
    }

    #[test]
    fn compute_task_ref_left_pads_uuid_into_low_16_bytes() {
        // 16 zero bytes ++ the task UUID's 16 raw bytes (gh#95 server verifier).
        let id = uuid::Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let task_ref = compute_task_ref(id);
        let bytes: [u8; 32] = task_ref.into();
        assert_eq!(&bytes[..16], &[0u8; 16], "high 16 bytes must be zero");
        assert_eq!(&bytes[16..], id.as_bytes(), "low 16 bytes must be the UUID");
    }

    #[test]
    fn open_calldata_has_expected_selector() {
        let call = TaskEscrow::openCall {
            token: Address::ZERO,
            deposit: U256::from(1u64),
            worker: Address::ZERO,
            platformFeeAmount: U256::from(0u64),
            platform: Address::ZERO,
            arbitrator: Address::ZERO,
            salt: B256::ZERO,
        };
        let data = call.abi_encode();
        // The `sol!`-generated selector must equal keccak(canonical signature)[..4].
        // These are independent representations: `data[0..4]` is derived by the
        // macro from the bound `open(...)` ABI above, while the string is the
        // canonical Solidity signature. Arg-order / type drift on either side
        // (e.g. dropping the new `address arbitrator`) fails this assert.
        let expected =
            &keccak256("open(address,uint256,address,uint256,address,address,bytes32)")[..4];
        assert_eq!(&data[0..4], expected);
    }
}
