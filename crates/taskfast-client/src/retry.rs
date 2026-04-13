use std::future::Future;
use std::time::Duration;

use crate::errors::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 3, base_delay: Duration::from_millis(500) }
    }
}

pub async fn with_backoff<T, F, Fut>(policy: RetryPolicy, mut op: F) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 1u32;
    loop {
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(Error::Server(_)) | Err(Error::Network(_)) if attempt < policy.max_attempts => {
                let delay = policy.base_delay * 2u32.pow(attempt - 1);
                tracing::warn!(attempt, ?delay, "retrying transient error");
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(Error::RateLimited { retry_after }) if attempt < policy.max_attempts => {
                tracing::warn!(attempt, ?retry_after, "rate limited, honoring retry_after");
                tokio::time::sleep(retry_after).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}
