//! Bounding a future's wall-clock time.

use std::future::Future;
use std::time::Duration;

use thiserror::Error;

/// The wrapped operation did not complete within the deadline.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("operation timed out after {0:?}")]
pub struct Elapsed(pub Duration);

/// Runs `fut`, failing with [`Elapsed`] if it does not resolve within
/// `duration`. A timeout is a fault, not a success — callers must treat the
/// `Err` branch exactly like any other failure of the underlying call
/// (retry it, trip a breaker, or deny, as appropriate) rather than
/// substituting a default value.
pub async fn with_timeout<T, Fut>(duration: Duration, fut: Fut) -> Result<T, Elapsed>
where
    Fut: Future<Output = T>,
{
    tokio::time::timeout(duration, fut)
        .await
        .map_err(|_| Elapsed(duration))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completes_under_deadline() {
        let result = with_timeout(Duration::from_millis(50), async { 42 }).await;
        assert_eq!(result, Ok(42));
    }

    #[tokio::test]
    async fn times_out_past_deadline() {
        let result = with_timeout(Duration::from_millis(5), async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            42
        })
        .await;
        assert_eq!(result, Err(Elapsed(Duration::from_millis(5))));
    }
}
