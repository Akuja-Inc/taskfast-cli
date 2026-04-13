//! Webhook registration + HMAC-SHA256 signature verification.
//!
//! Signed payload: `timestamp + "." + body` with a 5-minute replay window.
