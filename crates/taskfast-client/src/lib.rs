// SPDX-License-Identifier: MIT
//! TaskFast typed HTTP client.
//!
//! The `api` module is generated from `spec/openapi.yaml` at build time by
//! `build.rs`. The rewritten spec (error-alias folding) is produced in-memory
//! via `xtask::normalize_spec` so the on-disk spec stays authoritative.
//!
//! Use [`api::Client`] to issue requests; cross-cutting concerns
//! ([`errors::Error`], [`retry::with_backoff`]) live in sibling modules and
//! will be composed over the generated client in a follow-up.

pub mod client;
pub mod errors;
pub mod retry;

pub use client::{
    map_api_error, NetworkConfigEntry, NetworkConfigResponse, TaskFastClient, UserProfile,
};
pub use errors::{Error, Result};
pub use retry::{with_backoff, RetryPolicy};

/// Re-exported so downstream crates can call the generated client's
/// `baseurl()` / `client()` accessors — progenitor 0.11+ moved them off the
/// inherent impl into this trait, which must be in scope at the call site.
pub use progenitor_client::ClientInfo;

/// Convert an `i64` page limit (as parsed from the CLI) into the
/// `Option<NonZeroU64>` the generated client now requires: progenitor 0.14 /
/// typify encodes the spec's `minimum: 1` in the type. Non-positive limits
/// map to `None`, letting the server apply its default page size.
pub fn page_limit(n: i64) -> Option<std::num::NonZeroU64> {
    u64::try_from(n).ok().and_then(std::num::NonZeroU64::new)
}

/// Generated typed client + DTOs for the TaskFast OpenAPI spec.
///
/// Produced by `progenitor` from `spec/openapi.yaml` at build time; see
/// `build.rs` and `xtask::normalize_spec`. Do not edit by hand — regenerate
/// by changing the spec.
#[allow(
    clippy::all,
    clippy::pedantic,
    dead_code,
    irrefutable_let_patterns,
    missing_docs,
    non_camel_case_types,
    non_snake_case,
    renamed_and_removed_lints,
    unknown_lints,
    rustdoc::broken_intra_doc_links,
    rustdoc::invalid_html_tags
)]
pub mod api {
    include!(concat!(env!("OUT_DIR"), "/codegen.rs"));
}
