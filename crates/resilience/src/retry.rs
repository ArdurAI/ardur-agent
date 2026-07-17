//! Retry with exponential backoff and full jitter.
//!
//! Field names mirror `ardur_automation`'s workflow-engine `RetryPolicy`
//! (`crates/automation/src/tasks/flow/mod.rs`) so the two converge onto one
//! shape when the workflow `Retry` node adopts this crate.

use std::future::Future;
use std::time::Duration;

use rand::RngExt;

/// Backoff schedule for a retried operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum number of attempts, including the first attempt. `1` disables
    /// retrying entirely.
    pub max_attempts: u32,
    /// Initial backoff delay in milliseconds.
    pub initial_backoff_ms: u64,
    /// Maximum backoff delay in milliseconds — the exponential growth is
    /// capped here before jitter is applied.
    pub max_backoff_ms: u64,
    /// Multiplier applied to the backoff between attempts.
    pub backoff_multiplier: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff_ms: 100,
            max_backoff_ms: 10_000,
            backoff_multiplier: 2,
        }
    }
}

impl RetryPolicy {
    /// A policy that never retries — the first failure is final. Used as the
    /// safe default for callers that have not opted into retrying (e.g.
    /// call sites gating a security decision, where blind retrying is not
    /// automatically the right behavior).
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
            backoff_multiplier: 1,
        }
    }

    /// Backoff delay before the given zero-indexed retry attempt (`0` is the
    /// delay before the second overall attempt), before jitter.
    fn base_delay_ms(&self, retry_index: u32) -> u64 {
        let mut delay = self.initial_backoff_ms;
        for _ in 0..retry_index {
            delay = delay.saturating_mul(self.backoff_multiplier as u64);
            if delay >= self.max_backoff_ms {
                return self.max_backoff_ms;
            }
        }
        delay.min(self.max_backoff_ms)
    }

    /// Full-jitter delay (AWS "full jitter" algorithm): a uniform random
    /// value in `[0, base_delay]`, so retries spread out instead of
    /// synchronizing (thundering herd) after a shared outage.
    fn jittered_delay(&self, retry_index: u32) -> Duration {
        let base = self.base_delay_ms(retry_index);
        if base == 0 {
            return Duration::ZERO;
        }
        let jittered = rand::rng().random_range(0..=base);
        Duration::from_millis(jittered)
    }
}

/// Retries `op` under `policy`, calling `is_retryable` on each error to
/// decide whether another attempt should be made. Returns the last error if
/// attempts are exhausted.
///
/// `is_retryable` returning `false` stops retrying immediately and returns
/// that error — this is the hook callers on security-relevant paths must use
/// to make sure a "deny"/"indeterminate" outcome is never retried into an
/// accidental allow; see the crate-level fail-closed tests.
pub async fn retry_with_backoff<T, E, F, Fut>(
    policy: &RetryPolicy,
    is_retryable: impl Fn(&E) -> bool,
    mut op: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let attempts = policy.max_attempts.max(1);
    let mut last_err = None;
    for attempt in 0..attempts {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                let retryable = is_retryable(&err);
                last_err = Some(err);
                if !retryable || attempt + 1 >= attempts {
                    break;
                }
                let delay = policy.jittered_delay(attempt);
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    Err(last_err.expect("loop runs at least once"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn base_delay_grows_then_caps() {
        let policy = RetryPolicy {
            max_attempts: 10,
            initial_backoff_ms: 100,
            max_backoff_ms: 1_000,
            backoff_multiplier: 2,
        };
        assert_eq!(policy.base_delay_ms(0), 100);
        assert_eq!(policy.base_delay_ms(1), 200);
        assert_eq!(policy.base_delay_ms(2), 400);
        assert_eq!(policy.base_delay_ms(3), 800);
        assert_eq!(policy.base_delay_ms(4), 1_000);
        assert_eq!(policy.base_delay_ms(10), 1_000);
    }

    #[tokio::test]
    async fn retries_up_to_max_attempts_then_fails() {
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff_ms: 1,
            max_backoff_ms: 2,
            backoff_multiplier: 2,
        };
        let calls = AtomicU32::new(0);
        let result: Result<(), &'static str> = retry_with_backoff(
            &policy,
            |_| true,
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err("always fails") }
            },
        )
        .await;
        assert_eq!(result, Err("always fails"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn succeeds_after_transient_failures() {
        let policy = RetryPolicy {
            max_attempts: 5,
            initial_backoff_ms: 1,
            max_backoff_ms: 2,
            backoff_multiplier: 2,
        };
        let calls = AtomicU32::new(0);
        let result: Result<u32, &'static str> = retry_with_backoff(
            &policy,
            |_| true,
            || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                async move { if n < 2 { Err("transient") } else { Ok(n) } }
            },
        )
        .await;
        assert_eq!(result, Ok(2));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn non_retryable_error_stops_immediately() {
        let policy = RetryPolicy {
            max_attempts: 5,
            initial_backoff_ms: 1,
            max_backoff_ms: 2,
            backoff_multiplier: 2,
        };
        let calls = AtomicU32::new(0);
        let result: Result<(), &'static str> = retry_with_backoff(
            &policy,
            |_| false,
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err("deny — not a transient fault") }
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a non-retryable error (e.g. a policy denial) must not be retried"
        );
    }

    #[tokio::test]
    async fn none_policy_never_retries() {
        let calls = AtomicU32::new(0);
        let result: Result<(), &'static str> = retry_with_backoff(
            &RetryPolicy::none(),
            |_| true,
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err("fails") }
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
