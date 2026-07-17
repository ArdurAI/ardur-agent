//! Proves the fail-closed contract described in `src/lib.rs`: a
//! security-relevant check (stand-in for `ardur-cap-token`/`ardur-cedar-policy`
//! verification) wrapped in these resilience combinators must resolve to a
//! deny under every kind of injected fault — it must never silently resolve
//! to an allow just because the underlying check was slow, erroring, or
//! behind an open breaker.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ardur_resilience::{
    circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitError},
    retry::{RetryPolicy, retry_with_backoff},
    timeout::{Elapsed, with_timeout},
};

/// A minimal stand-in for a capability/policy decision, mirroring
/// `ardur_cedar_policy::Decision`'s two failure variants folding into one
/// "not allowed" bucket at the call site.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Allow,
    Deny,
}

/// Mirrors how a real caller (e.g. `fused-runtime`'s `stage_cedar_with_action`)
/// treats "the check produced an error" — as deny, never allow.
fn decide<E>(result: Result<Decision, E>) -> Decision {
    match result {
        Ok(decision) => decision,
        Err(_) => Decision::Deny,
    }
}

#[tokio::test]
async fn timeout_on_a_hung_policy_check_denies() {
    let policy_check = async {
        // Simulates a policy backend that never responds.
        tokio::time::sleep(Duration::from_secs(10)).await;
        Decision::Allow
    };

    let result: Result<Decision, Elapsed> =
        with_timeout(Duration::from_millis(10), policy_check).await;

    assert_eq!(decide(result), Decision::Deny);
}

#[tokio::test]
async fn exhausted_retries_against_a_failing_policy_check_denies() {
    let calls = AtomicU32::new(0);
    let policy = RetryPolicy {
        max_attempts: 3,
        initial_backoff_ms: 1,
        max_backoff_ms: 2,
        backoff_multiplier: 2,
    };

    let result: Result<Decision, &'static str> = retry_with_backoff(
        &policy,
        // A real caller would mark policy-denial errors non-retryable; here
        // we mark the fault retryable to prove that even with retrying
        // enabled, exhausting attempts still surfaces as Err, not a
        // default-constructed Allow.
        |_| true,
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err("policy backend unreachable") }
        },
    )
    .await;

    assert_eq!(decide(result), Decision::Deny);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn open_breaker_denies_without_invoking_the_check() {
    let breaker = CircuitBreaker::new(CircuitBreakerConfig {
        failure_threshold: 1,
        open_duration: Duration::from_secs(60),
    });
    let calls = AtomicU32::new(0);

    // Trip the breaker.
    let tripped: Result<Decision, CircuitError<&'static str>> = breaker
        .call(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err("policy backend down") }
        })
        .await;
    assert_eq!(decide(tripped), Decision::Deny);

    // While open, the wrapped check must not even run — and the outcome is
    // still a deny, never a cached/stale allow.
    let result: Result<Decision, CircuitError<&'static str>> = breaker
        .call(|| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(Decision::Allow) }
        })
        .await;

    assert_eq!(decide(result), Decision::Deny);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the policy check must not be invoked while the breaker is open"
    );
}

/// Folds the two nested failure layers (breaker-open, or the inner op's own
/// error) into the same "deny" bucket a real caller would use.
fn decide_composed<E>(result: Result<Decision, CircuitError<E>>) -> Decision {
    match result {
        Ok(decision) => decision,
        Err(_) => Decision::Deny,
    }
}

#[tokio::test]
async fn composed_timeout_plus_breaker_still_denies() {
    // A realistic wiring: each call is time-bounded, and repeated timeouts
    // trip a breaker that then fails fast. Every layer must forward Err.
    let breaker = CircuitBreaker::new(CircuitBreakerConfig {
        failure_threshold: 2,
        open_duration: Duration::from_secs(60),
    });

    for _ in 0..2 {
        let result: Result<Decision, CircuitError<Elapsed>> = breaker
            .call(|| {
                with_timeout(Duration::from_millis(5), async {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    Decision::Allow
                })
            })
            .await;
        assert_eq!(decide_composed(result), Decision::Deny);
    }

    // Now open; further calls must deny fast without running the timeout body.
    let final_result: Result<Decision, CircuitError<Elapsed>> =
        breaker.call(|| async { Ok(Decision::Allow) }).await;
    assert_eq!(decide_composed(final_result), Decision::Deny);
}
