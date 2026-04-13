//! Agent-layer retry loops (distinct from HTTP transport retry in taskfast-client).
//!
//! Honors `RateLimited { retry_after }` from the client error enum.
