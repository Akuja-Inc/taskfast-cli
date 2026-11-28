// SPDX-License-Identifier: MIT
//! Webhook registration + HMAC-SHA256 signature verification.
//!
//! One offline verifier ([`verify_signature`]) that callers use inside their HTTP
//! handler to authenticate incoming platform deliveries without
//! round-tripping back to `POST /webhooks/verify`.
//!
//! # Signature format
//!
//! The platform (see `lib/task_fast/marketplace/webhook_delivery.ex`)
//! sends each event with:
//!
//! - `X-Webhook-Signature` — lowercase hex of
//!   `HMAC_SHA256(secret, timestamp ++ "." ++ body)`
//! - `X-Webhook-Timestamp` — ISO 8601 UTC
//! - `X-Webhook-Event`     — event type name
//!
//! Verification re-hashes the canonical string and constant-time-compares
//! via [`hmac::Mac::verify_slice`]. Deliveries older than
//! [`VerifyOptions::max_skew`] (default 5 min) are rejected on timestamp
//! alone — this is the replay-protection window the platform docs
//! promise at `openapi.yaml:2365`.

use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use jiff::Timestamp;
use sha2::Sha256;
use thiserror::Error as ThisError;

type HmacSha256 = Hmac<Sha256>;

/// Default replay window matching the server-side contract.
pub const DEFAULT_MAX_SKEW: Duration = Duration::from_mins(5);

/// Tunables for [`verify_signature`]. `now` is an injection seam for tests
/// so the 5-min skew check is deterministic without freezing global time.
#[derive(Debug, Clone, Copy)]
pub struct VerifyOptions {
    pub max_skew: Duration,
    pub now: Option<Timestamp>,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            max_skew: DEFAULT_MAX_SKEW,
            now: None,
        }
    }
}

/// Reasons [`verify_signature`] rejects a delivery. Split from
/// [`taskfast_client::Error`] because signature verification is a pure,
/// offline operation — it never interacts with the HTTP stack, so it
/// shouldn't surface as a network/server-flavored error variant.
#[derive(Debug, ThisError)]
pub enum SignatureError {
    #[error("signature timestamp not valid ISO 8601: {0}")]
    InvalidTimestamp(String),
    #[error("signature header is not valid hex")]
    InvalidSignatureHex,
    #[error("secret is empty")]
    EmptySecret,
    #[error("signature mismatch")]
    Mismatch,
    #[error("timestamp skew {skew_secs}s exceeds max {max_secs}s")]
    TimestampSkewed { skew_secs: i64, max_secs: i64 },
}

/// Verify an incoming webhook delivery's `X-Webhook-Signature` header.
///
/// Canonical signed string is `{timestamp}.{body}`. `timestamp` is the raw
/// `X-Webhook-Timestamp` header value — we hash the exact bytes the sender
/// hashed, then parse it separately for the freshness check so malformed
/// timestamps fail loudly rather than being coerced.
pub fn verify_signature(
    secret: &str,
    timestamp: &str,
    body: &str,
    signature_hex: &str,
    opts: VerifyOptions,
) -> std::result::Result<(), SignatureError> {
    if secret.is_empty() {
        return Err(SignatureError::EmptySecret);
    }

    let parsed_ts: Timestamp = timestamp
        .parse()
        .map_err(|_| SignatureError::InvalidTimestamp(timestamp.to_string()))?;
    let now = opts.now.unwrap_or_else(Timestamp::now);
    let skew_secs = (now.as_second() - parsed_ts.as_second()).abs();
    let max_secs = opts.max_skew.as_secs() as i64;
    if skew_secs > max_secs {
        return Err(SignatureError::TimestampSkewed {
            skew_secs,
            max_secs,
        });
    }

    let provided = hex::decode(signature_hex).map_err(|_| SignatureError::InvalidSignatureHex)?;

    // Mac::new_from_slice accepts any key length for HMAC; only length-0
    // secrets are a protocol violation and we've already rejected those.
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| SignatureError::EmptySecret)?;
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    mac.verify_slice(&provided)
        .map_err(|_| SignatureError::Mismatch)
}

/// Build the canonical signature hex for a given payload. Exposed so
/// callers can produce matching signatures in their own tests without
/// reimplementing the canonical-string rules.
pub fn sign_payload(secret: &str, timestamp: &str, body: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC should accept non-empty secret");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_test_1234567890";
    const TS: &str = "2026-03-23T12:00:00Z";
    const BODY: &str =
        r#"{"event":"test","timestamp":"2026-03-23T12:00:00Z","data":{"message":"ok"}}"#;

    fn fixed_now() -> Timestamp {
        "2026-03-23T12:01:00Z".parse().unwrap()
    }

    fn opts_at(now: Timestamp) -> VerifyOptions {
        VerifyOptions {
            max_skew: DEFAULT_MAX_SKEW,
            now: Some(now),
        }
    }

    #[test]
    fn sign_and_verify_roundtrip_accepts() {
        let sig = sign_payload(SECRET, TS, BODY);
        verify_signature(SECRET, TS, BODY, &sig, opts_at(fixed_now())).expect("valid signature");
    }

    #[test]
    fn known_vector_matches_platform_elixir_impl() {
        // Expected digest from Elixir's :crypto.mac/4 on the timestamp and raw body.
        assert_eq!(
            sign_payload(SECRET, TS, BODY),
            "4e7a86210e3e640bf1bbd677e03d97ab826a131fac66117b924e08fbcd3b1614"
        );
    }

    #[test]
    fn tampered_body_is_rejected() {
        let sig = sign_payload(SECRET, TS, BODY);
        let err = verify_signature(SECRET, TS, "{\"evil\":true}", &sig, opts_at(fixed_now()))
            .unwrap_err();
        assert!(matches!(err, SignatureError::Mismatch));
    }

    #[test]
    fn tampered_timestamp_is_rejected_as_mismatch_when_in_window() {
        // Different (but still-fresh) timestamp → hash mismatch, not skew.
        let sig = sign_payload(SECRET, TS, BODY);
        let other_ts = "2026-03-23T12:00:30Z";
        let err = verify_signature(SECRET, other_ts, BODY, &sig, opts_at(fixed_now())).unwrap_err();
        assert!(matches!(err, SignatureError::Mismatch));
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let sig = sign_payload(SECRET, TS, BODY);
        let err = verify_signature(
            "whsec_wrong_0000000000",
            TS,
            BODY,
            &sig,
            opts_at(fixed_now()),
        )
        .unwrap_err();
        assert!(matches!(err, SignatureError::Mismatch));
    }

    #[test]
    fn stale_timestamp_is_rejected_even_with_valid_signature() {
        let sig = sign_payload(SECRET, TS, BODY);
        // 6 minutes after the signed timestamp — past 5-min window.
        let late: Timestamp = "2026-03-23T12:06:01Z".parse().unwrap();
        let err = verify_signature(SECRET, TS, BODY, &sig, opts_at(late)).unwrap_err();
        assert!(matches!(err, SignatureError::TimestampSkewed { .. }));
    }

    #[test]
    fn malformed_timestamp_fails_before_hmac() {
        let sig = sign_payload(SECRET, TS, BODY);
        let err =
            verify_signature(SECRET, "not-a-date", BODY, &sig, opts_at(fixed_now())).unwrap_err();
        assert!(matches!(err, SignatureError::InvalidTimestamp(_)));
    }

    #[test]
    fn non_hex_signature_is_rejected() {
        let err =
            verify_signature(SECRET, TS, BODY, "not-hex-!!", opts_at(fixed_now())).unwrap_err();
        assert!(matches!(err, SignatureError::InvalidSignatureHex));
    }

    #[test]
    fn empty_secret_is_rejected() {
        let sig = sign_payload(SECRET, TS, BODY);
        let err = verify_signature("", TS, BODY, &sig, opts_at(fixed_now())).unwrap_err();
        assert!(matches!(err, SignatureError::EmptySecret));
    }

    #[test]
    fn constant_time_compare_rejects_truncated_signature() {
        let sig = sign_payload(SECRET, TS, BODY);
        let truncated = &sig[..sig.len() - 2];
        let err = verify_signature(SECRET, TS, BODY, truncated, opts_at(fixed_now())).unwrap_err();
        // Length mismatch surfaces as Mismatch (verify_slice rejects
        // wrong-length tags even before constant-time compare).
        assert!(matches!(err, SignatureError::Mismatch));
    }

    #[test]
    fn future_timestamp_beyond_window_is_rejected() {
        let sig = sign_payload(SECRET, TS, BODY);
        // Signed "in the future" relative to now; absolute skew > 5 min.
        let past_now: Timestamp = "2026-03-23T11:50:00Z".parse().unwrap();
        let err = verify_signature(SECRET, TS, BODY, &sig, opts_at(past_now)).unwrap_err();
        assert!(matches!(err, SignatureError::TimestampSkewed { .. }));
    }
}
