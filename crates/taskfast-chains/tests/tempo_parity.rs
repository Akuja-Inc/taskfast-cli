//! Parity suite ported from `taskfast-agent/src/signing.rs` tests (beads
//! am-6v7b.4). Byte-equality of the Elixir cross-check fixture is the critical
//! invariant — any drift forks signature recoverability.

#![cfg(feature = "tempo")]

use alloy_primitives::{Address, B256, U256};
use alloy_signer_local::PrivateKeySigner;
use taskfast_chains::tempo::{
    distribution_digest, sign_distribution, sign_hash_raw, verify_distribution,
    DistributionDomain, SigningError,
};

fn fixed_domain() -> DistributionDomain {
    let vc: Address = "0x00000000000000000000000000000000000000ee"
        .parse()
        .unwrap();
    DistributionDomain::testnet(vc)
}

fn fixed_escrow_id() -> B256 {
    let mut b = [0u8; 32];
    b[31] = 0x42;
    B256::from(b)
}

#[test]
fn chain_id_constructors_are_correct() {
    let vc = Address::ZERO;
    assert_eq!(DistributionDomain::mainnet(vc).chain_id, 4_217);
    assert_eq!(DistributionDomain::testnet(vc).chain_id, 42_431);
}

#[test]
fn digest_is_deterministic_for_same_inputs() {
    let d1 = distribution_digest(&fixed_domain(), fixed_escrow_id(), U256::from(100u64));
    let d2 = distribution_digest(&fixed_domain(), fixed_escrow_id(), U256::from(100u64));
    assert_eq!(d1, d2);
}

#[test]
fn digest_differs_when_chain_id_differs() {
    let vc: Address = "0x00000000000000000000000000000000000000ee"
        .parse()
        .unwrap();
    let testnet = DistributionDomain::testnet(vc);
    let mainnet = DistributionDomain::mainnet(vc);
    let a = distribution_digest(&testnet, fixed_escrow_id(), U256::from(100u64));
    let b = distribution_digest(&mainnet, fixed_escrow_id(), U256::from(100u64));
    assert_ne!(a, b, "chain_id must be bound into the domain separator");
}

#[test]
fn digest_differs_when_escrow_id_differs() {
    let domain = fixed_domain();
    let a = distribution_digest(&domain, fixed_escrow_id(), U256::from(100u64));
    let mut other = [0u8; 32];
    other[31] = 0x43;
    let b = distribution_digest(&domain, B256::from(other), U256::from(100u64));
    assert_ne!(a, b);
}

#[test]
fn signature_hex_has_expected_shape() {
    let signer = PrivateKeySigner::random();
    let sig = sign_distribution(
        &signer,
        &fixed_domain(),
        fixed_escrow_id(),
        U256::from(100u64),
    )
    .expect("sign");
    assert_eq!(sig.len(), 132);
    assert!(sig.starts_with("0x"));
    assert!(sig[2..].chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn sign_then_recover_roundtrip() {
    let signer = PrivateKeySigner::random();
    let expected = signer.address();
    let sig = sign_distribution(
        &signer,
        &fixed_domain(),
        fixed_escrow_id(),
        U256::from(1_700_000_000u64),
    )
    .expect("sign");
    let ok = verify_distribution(
        &sig,
        &fixed_domain(),
        fixed_escrow_id(),
        U256::from(1_700_000_000u64),
        expected,
    )
    .expect("verify");
    assert!(ok);
}

#[test]
fn tampered_deadline_fails_verification() {
    let signer = PrivateKeySigner::random();
    let expected = signer.address();
    let sig = sign_distribution(
        &signer,
        &fixed_domain(),
        fixed_escrow_id(),
        U256::from(1_700_000_000u64),
    )
    .expect("sign");
    let ok = verify_distribution(
        &sig,
        &fixed_domain(),
        fixed_escrow_id(),
        U256::from(1_700_000_001u64),
        expected,
    )
    .expect("verify");
    assert!(!ok, "tampered deadline must not verify against signer");
}

#[test]
fn tampered_escrow_id_fails_verification() {
    let signer = PrivateKeySigner::random();
    let expected = signer.address();
    let sig = sign_distribution(
        &signer,
        &fixed_domain(),
        fixed_escrow_id(),
        U256::from(100u64),
    )
    .expect("sign");
    let mut other = [0u8; 32];
    other[31] = 0x99;
    let ok = verify_distribution(
        &sig,
        &fixed_domain(),
        B256::from(other),
        U256::from(100u64),
        expected,
    )
    .expect("verify");
    assert!(!ok);
}

#[test]
fn cross_chain_replay_fails_verification() {
    let signer = PrivateKeySigner::random();
    let expected = signer.address();
    let vc: Address = "0x00000000000000000000000000000000000000ee"
        .parse()
        .unwrap();
    let testnet = DistributionDomain::testnet(vc);
    let mainnet = DistributionDomain::mainnet(vc);
    let sig = sign_distribution(&signer, &testnet, fixed_escrow_id(), U256::from(1u64))
        .expect("sign");
    let ok = verify_distribution(
        &sig,
        &mainnet,
        fixed_escrow_id(),
        U256::from(1u64),
        expected,
    )
    .expect("verify");
    assert!(!ok, "testnet signature must not verify on mainnet");
}

#[test]
fn sign_hash_raw_roundtrips_via_prehash_recovery() {
    use alloy_primitives::Signature;

    let signer = PrivateKeySigner::random();
    let expected = signer.address();
    let mut digest_bytes = [0u8; 32];
    digest_bytes[0] = 0xde;
    digest_bytes[1] = 0xad;
    digest_bytes[2] = 0xbe;
    digest_bytes[3] = 0xef;
    let digest = B256::from(digest_bytes);

    let sig_hex = sign_hash_raw(&signer, digest).expect("sign");
    let stripped = sig_hex.strip_prefix("0x").unwrap();
    let bytes = hex::decode(stripped).expect("decode");
    let sig = Signature::try_from(bytes.as_slice()).expect("parse");
    let recovered = sig.recover_address_from_prehash(&digest).expect("recover");
    assert_eq!(recovered, expected);
}

#[test]
fn verify_rejects_malformed_signature_hex() {
    let err = verify_distribution(
        "0xnothex",
        &fixed_domain(),
        fixed_escrow_id(),
        U256::from(1u64),
        Address::ZERO,
    )
    .unwrap_err();
    assert!(matches!(err, SigningError::InvalidSignatureHex(_)));
}

#[test]
fn verify_rejects_wrong_length_signature() {
    let short = format!("0x{}", "ab".repeat(32));
    let err = verify_distribution(
        &short,
        &fixed_domain(),
        fixed_escrow_id(),
        U256::from(1u64),
        Address::ZERO,
    )
    .unwrap_err();
    assert!(matches!(err, SigningError::InvalidSignatureHex(_)));
}

/// Cross-check fixture vs. Elixir `DistributionApprovalVerifier` digest.
/// Byte-equality is load-bearing — drift on either side breaks recoverability.
#[test]
fn cross_check_digest_matches_elixir_fixture() {
    let vc: Address = "0x0000000000000000000000000000000000000001"
        .parse()
        .unwrap();
    let domain = DistributionDomain::testnet(vc);
    let mut escrow_bytes = [0u8; 32];
    escrow_bytes.iter_mut().for_each(|b| *b = 0xab);
    let escrow_id = B256::from(escrow_bytes);
    let deadline = U256::from(1_800_000_000u64);

    let digest = distribution_digest(&domain, escrow_id, deadline);
    let hex = format!("0x{}", hex::encode(digest.as_slice()));

    assert_eq!(hex.len(), 66);
    assert_eq!(
        hex, "0xff4958335cd476ae06389497e736d3630ecee1b9b33cc65cbfd9c316dd2e3efb",
        "digest drifted — Elixir mirror must update in lockstep"
    );
}
