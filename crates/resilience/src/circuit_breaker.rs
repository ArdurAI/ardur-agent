//! A self-expiring circuit breaker.
//!
//! The open state expires on its own after `open_duration` — mirroring the
//! TTL-based self-healing already used by `ardur-cost-gate`'s reservation
//! expiry (`crates/cost-gate/src/gate.rs`) — rather than requiring an
//! external operator to reset it. There is no way to force a breaker closed
//! from the outside; it only closes by a probe call succeeding.

use std::future::Future;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use thiserror::Error;

/// Tunables for a [`CircuitBreaker`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CircuitBreakerConfig {
    /// Consecutive failures (from the closed state) before the breaker opens.
    pub failure_threshold: u32,
    /// How long the breaker stays open before allowing a single half-open
    /// probe call through.
    pub open_duration: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_duration: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Closed {
        consecutive_failures: u32,
    },
    Open {
        until: Instant,
    },
    /// A single probe call is in flight; further calls are rejected until it
    /// resolves.
    HalfOpen,
}

/// A label describing the breaker's current disposition, safe to surface in
/// a health check (see `ardur-health`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

/// Error returned by [`CircuitBreaker::call`].
#[derive(Debug, Error)]
pub enum CircuitError<E> {
    /// The breaker is open (or a half-open probe is already in flight): the
    /// wrapped operation was **not invoked**. This is the fail-fast/fail-closed
    /// guarantee — callers gating a security decision behind a breaker see an
    /// `Err` here, never a stale cached "allow".
    #[error("circuit breaker open")]
    Open,
    /// The operation ran and failed on its own terms.
    #[error(transparent)]
    Inner(E),
}

#[derive(Debug)]
pub struct CircuitBreaker {
    state: Mutex<State>,
    config: CircuitBreakerConfig,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            state: Mutex::new(State::Closed {
                consecutive_failures: 0,
            }),
            config,
        }
    }

    /// Current state, for health reporting.
    pub fn state(&self) -> CircuitState {
        match *self.state.lock() {
            State::Closed { .. } => CircuitState::Closed,
            State::Open { until } if Instant::now() < until => CircuitState::Open,
            // Past its TTL, an "Open" state reads as half-open: the next
            // call() will consume the probe slot.
            State::Open { .. } => CircuitState::HalfOpen,
            State::HalfOpen => CircuitState::HalfOpen,
        }
    }

    /// Runs `op` through the breaker. Returns [`CircuitError::Open`] without
    /// invoking `op` at all when the breaker is tripped and still cooling
    /// down.
    pub async fn call<T, E, F, Fut>(&self, op: F) -> Result<T, CircuitError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        if !self.admit() {
            return Err(CircuitError::Open);
        }
        match op().await {
            Ok(value) => {
                self.on_success();
                Ok(value)
            }
            Err(err) => {
                self.on_failure();
                Err(CircuitError::Inner(err))
            }
        }
    }

    /// Decides whether a call may proceed, transitioning `Open -> HalfOpen`
    /// in place when the cooldown has elapsed. Returns `false` when the
    /// breaker must reject without running the operation.
    fn admit(&self) -> bool {
        let mut state = self.state.lock();
        match *state {
            State::Closed { .. } => true,
            State::Open { until } => {
                if Instant::now() >= until {
                    *state = State::HalfOpen;
                    true
                } else {
                    false
                }
            }
            State::HalfOpen => false,
        }
    }

    fn on_success(&self) {
        let mut state = self.state.lock();
        *state = State::Closed {
            consecutive_failures: 0,
        };
    }

    fn on_failure(&self) {
        let mut state = self.state.lock();
        *state = match *state {
            State::Closed {
                consecutive_failures,
            } => {
                let failures = consecutive_failures + 1;
                if failures >= self.config.failure_threshold {
                    State::Open {
                        until: Instant::now() + self.config.open_duration,
                    }
                } else {
                    State::Closed {
                        consecutive_failures: failures,
                    }
                }
            }
            State::HalfOpen => State::Open {
                until: Instant::now() + self.config.open_duration,
            },
            open @ State::Open { .. } => open,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn config(threshold: u32, open_for: Duration) -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: threshold,
            open_duration: open_for,
        }
    }

    #[tokio::test]
    async fn opens_after_threshold_and_rejects_without_calling_op() {
        let breaker = CircuitBreaker::new(config(3, Duration::from_secs(60)));
        let calls = AtomicU32::new(0);

        for _ in 0..3 {
            let result: Result<(), CircuitError<&'static str>> = breaker
                .call(|| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Err("boom") }
                })
                .await;
            assert!(matches!(result, Err(CircuitError::Inner("boom"))));
        }
        assert_eq!(breaker.state(), CircuitState::Open);

        // A fourth call must be rejected fast, without invoking op at all —
        // this is the fail-closed guarantee under an open breaker.
        let result: Result<(), CircuitError<&'static str>> = breaker
            .call(|| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            })
            .await;
        assert!(matches!(result, Err(CircuitError::Open)));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "op must not run while the breaker is open"
        );
    }

    #[tokio::test]
    async fn half_open_probe_recovers_the_breaker() {
        let breaker = CircuitBreaker::new(config(1, Duration::from_millis(10)));

        let failed: Result<(), CircuitError<&'static str>> =
            breaker.call(|| async { Err("boom") }).await;
        assert!(matches!(failed, Err(CircuitError::Inner("boom"))));
        assert_eq!(breaker.state(), CircuitState::Open);

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(breaker.state(), CircuitState::HalfOpen);

        let recovered: Result<u32, CircuitError<&'static str>> =
            breaker.call(|| async { Ok(7) }).await;
        assert_eq!(recovered.unwrap(), 7);
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[tokio::test]
    async fn failed_probe_reopens_immediately() {
        let breaker = CircuitBreaker::new(config(1, Duration::from_millis(10)));
        let _: Result<(), CircuitError<&'static str>> =
            breaker.call(|| async { Err("boom") }).await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        let probe: Result<(), CircuitError<&'static str>> =
            breaker.call(|| async { Err("still broken") }).await;
        assert!(matches!(probe, Err(CircuitError::Inner("still broken"))));
        assert_eq!(breaker.state(), CircuitState::Open);
    }

    #[tokio::test]
    async fn single_failure_below_threshold_stays_closed() {
        let breaker = CircuitBreaker::new(config(5, Duration::from_secs(60)));
        let _: Result<(), CircuitError<&'static str>> =
            breaker.call(|| async { Err("blip") }).await;
        assert_eq!(breaker.state(), CircuitState::Closed);
    }
}
