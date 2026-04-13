//! TaskFast typed HTTP client.
//!
//! Phase 1 scaffold: exports the error taxonomy + retry policy only.
//! Progenitor-generated endpoints land in a follow-up task (see am-9oj deps).

pub mod errors;
pub mod retry;

pub use errors::{Error, Result};
pub use retry::{RetryPolicy, with_backoff};
