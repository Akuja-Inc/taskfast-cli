#![no_main]
//! Fuzz the `.taskfast/config.json` deserializer.
//!
//! `Config::load` runs on untrusted input: it parses config files that ship
//! inside cloned repos, which the F2 endpoint guard explicitly treats as
//! attacker-controlled. `load` itself does file I/O around a path, so we fuzz
//! the serde layer it delegates to — `serde_json::from_slice::<Config>` — which
//! is where a panic, unbounded recursion, or parser mismatch would surface.

use libfuzzer_sys::fuzz_target;
use taskfast_cli::Config;

fuzz_target!(|data: &[u8]| {
    let _ = serde_json::from_slice::<Config>(data);
});
