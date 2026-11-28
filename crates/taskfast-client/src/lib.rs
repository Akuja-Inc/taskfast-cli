// SPDX-License-Identifier: MIT
//! TaskFast typed HTTP client.
//!
//! `build.rs` uses `taskfast_codegen::normalize_spec` and progenitor to generate
//! [`api`] from `spec/openapi.yaml` without changing the spec on disk.
//!
//! [`TaskFastClient`] wraps [`api::Client`] with authentication and retry support.
//! [`map_api_error`] converts generated client errors into [`Error`].

pub mod client;
pub mod corr;
pub mod errors;
pub mod retry;

pub use client::{
    map_api_error, put_presigned_upload, ArtifactUpload, NetworkConfigEntry, NetworkConfigResponse,
    NewAgentOutcome, TaskFastClient, UserProfile,
};
// `record_corr` is the generated client's post-hook body (via the
// `ClientHooks` impl in corr.rs) and is re-exported for hand-rolled callers.
pub use corr::{record_corr, record_corr_response, take_last_corr};
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

#[cfg(test)]
mod tests {
    /// typify matches schema→type conversions by exact `SchemaObject`
    /// equality, so a spec date that grows an extra constraint silently
    /// reverts to chrono; this pins the gh#235 migration.
    #[test]
    fn generated_client_maps_dates_to_jiff() {
        let codegen = std::fs::read_to_string(env!("TASKFAST_CODEGEN_PATH")).unwrap();
        assert!(
            !codegen.contains("chrono::"),
            "generated client drifted back to chrono (gh#235)"
        );
        assert!(codegen.contains("::jiff::Timestamp"));
        assert!(codegen.contains("::jiff::civil::Date"));
    }
}

/// Generated typed client + DTOs for the TaskFast OpenAPI spec.
///
/// `build.rs` normalizes `spec/openapi.yaml` with `taskfast_codegen::normalize_spec`
/// and passes it to progenitor at build time.
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
