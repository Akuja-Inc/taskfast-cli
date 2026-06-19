// SPDX-License-Identifier: MIT
#![no_main]
//! Fuzz decoding of untrusted API response bodies.
//!
//! Issue #32 lists "envelope decoding" as a target. The CLI's output
//! [`taskfast_cli::Envelope`] is `Serialize`-only (the CLI writes it, never
//! reads it), so it is not a decode surface. The real untrusted-decode path is
//! the hand-rolled response types in `taskfast-client` that are
//! `serde_json::from_slice`-d straight off the wire: the network-config cache
//! (`GET /config/network`) and the user profile (`GET /users/me`).

use libfuzzer_sys::fuzz_target;
use taskfast_client::{NetworkConfigResponse, UserProfile};

fuzz_target!(|data: &[u8]| {
    let _ = serde_json::from_slice::<NetworkConfigResponse>(data);
    let _ = serde_json::from_slice::<UserProfile>(data);
});
