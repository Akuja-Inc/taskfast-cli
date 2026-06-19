#![no_main]
//! Fuzz the Tempo chain-id + fee-token validators.
//!
//! These run on values pulled from config, CLI flags, and server responses —
//! all attacker-influenced. `is_allowed_fee_token` does case-insensitive hex
//! normalization on an arbitrary string, the most likely panic site.
//!
//! Input layout: first 8 bytes (little-endian) → `chain_id`; the remainder,
//! when valid UTF-8, → candidate fee-token hex string.

use libfuzzer_sys::fuzz_target;
use taskfast_chains::tempo::{is_allowed_fee_token, is_known_network};

fuzz_target!(|data: &[u8]| {
    let (chain_id, rest) = match data.split_first_chunk::<8>() {
        Some((head, tail)) => (u64::from_le_bytes(*head), tail),
        None => (0u64, data),
    };
    let _ = is_known_network(chain_id);
    if let Ok(token) = std::str::from_utf8(rest) {
        let _ = is_allowed_fee_token(chain_id, token);
    }
});
