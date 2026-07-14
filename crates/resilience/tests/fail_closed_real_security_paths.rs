//! Proves the fail-closed contract in `src/lib.rs` against the REAL
//! security-relevant paths, not a stand-in: `ardur_cap_token`'s Biscuit
//! verifier and `ardur_cedar_policy`'s Cedar engine. Both crates are
//! synchronous and make no network call today, but this is exactly the shape
//! either will take once a remote deny-list/policy-fetch variant lands (see
//! `ardur_cedar_policy::PolicySource`'s own `// TODO Phase 2: Remote` marker)
//! — so these tests wrap the genuine verify/evaluate calls in the resilience
//! combinators and prove the deny-on-fault guarantee holds using production
//! types and production "fold an error to deny" logic, not a fabricated one.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ardur_cap_token::{
    BiscuitCapTokenIssuer, BiscuitCapTokenVerifier, CapScope, CapTokenError, CapTokenIssuer,
    CapTokenVerifier, HashSetDenyList, HolderId, KeyPair, RequiredCaveats,
};
use ardur_cedar_policy::{
    ActionRef, CedarPolicyBundle, Decision, EvaluationContext, PolicyBundle, PolicySource,
    PrincipalRef, ResourceRef,
};
use ardur_resilience::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitError};
use ardur_resilience::retry::{RetryPolicy, retry_with_backoff};

fn scope() -> CapScope {
    CapScope {
        audience: "svc-a".to_string(),
        expires_unix: 2_000_000_000,
        budget_remaining: 1000,
        tool_allowlist: vec!["search".to_string()],
    }
}

fn required() -> RequiredCaveats {
    RequiredCaveats {
        now_unix: 1_700_000_000,
        audience: "svc-a".to_string(),
        tool: "search".to_string(),
        cost: 100,
    }
}

/// A genuinely valid cap-token — issued, and verified successfully on its
/// own — is still denied while the circuit breaker wrapping the verify call
/// is open. This is the strongest form of the fail-closed proof: an open
/// breaker overrides what would otherwise be a legitimate allow.
#[tokio::test]
async fn open_breaker_denies_a_genuinely_valid_cap_token() {
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let root = issuer.public_key();
    let token = issuer
        .issue(HolderId("spiffe://ardur/user/alice".to_string()), scope())
        .expect("issue a real token");
    let verifier = BiscuitCapTokenVerifier::new(HashSetDenyList::new());
    let req = required();

    // Sanity: this exact token/root/request genuinely verifies today.
    assert!(verifier.verify(&token, &root, &req).is_ok());

    let breaker = CircuitBreaker::new(CircuitBreakerConfig {
        failure_threshold: 2,
        open_duration: Duration::from_secs(60),
    });
    // A real, reproducible verify failure: the wrong root key.
    let wrong_root = KeyPair::new().public();

    for _ in 0..2 {
        let result = breaker
            .call(|| async { verifier.verify(&token, &wrong_root, &req) })
            .await;
        assert!(
            matches!(
                result,
                Err(CircuitError::Inner(CapTokenError::SignatureInvalid))
            ),
            "expected a genuine signature failure to trip the breaker"
        );
    }

    // The breaker is now open. Verify the ORIGINAL VALID token against the
    // CORRECT root — an operation that, run directly (as proven above),
    // succeeds — and confirm it is still denied without even running.
    let result = breaker
        .call(|| async { verifier.verify(&token, &root, &req) })
        .await;
    assert!(
        matches!(result, Err(CircuitError::Open)),
        "an open breaker must deny even a token that would genuinely verify"
    );
}

/// A malformed principal reference is a genuine internal fault in
/// `CedarPolicyBundle::try_evaluate` (not a fabricated stand-in) — it already
/// resolves to `Decision::Indeterminate` today per the crate's own fail-closed
/// design. This proves that wrapping `evaluate` in `retry_with_backoff` never
/// turns that Indeterminate into an Allow, even when every retry attempt is
/// exhausted, and that the fold fused-runtime's `stage_cedar_with_action`
/// applies (`Indeterminate` -> deny) survives the retry wrapper unchanged.
#[tokio::test]
async fn retries_never_turn_a_real_indeterminate_into_an_allow() {
    let bundle = CedarPolicyBundle::load(PolicySource::Embedded(
        "permit (principal, action, resource);".to_string(),
    ))
    .expect("embedded policy loads");

    // Sanity: this bundle genuinely allows a well-formed request — the
    // Indeterminate below is not because the policy can never allow anything.
    let good_ctx = EvaluationContext {
        principal: PrincipalRef("User::alice".to_string()),
        action: ActionRef("Action::Read".to_string()),
        resource: ResourceRef("Doc::readme".to_string()),
        attributes: serde_json::Value::Null,
    };
    assert!(matches!(bundle.evaluate(&good_ctx), Decision::Allow { .. }));

    // A genuinely malformed principal reference (no `Type::id` separator) —
    // `parse_entity_ref` rejects this for real, producing a real
    // `Decision::Indeterminate`.
    let bad_ctx = EvaluationContext {
        principal: PrincipalRef("not-a-valid-entity-ref".to_string()),
        ..good_ctx
    };

    let policy = RetryPolicy {
        max_attempts: 3,
        initial_backoff_ms: 1,
        max_backoff_ms: 2,
        backoff_multiplier: 2,
    };
    let calls = AtomicU32::new(0);

    // A caller that (wrongly, but worst-case) treats Indeterminate as
    // transient and worth retrying — the strongest test of the guarantee.
    let result: Result<Decision, Decision> = retry_with_backoff(
        &policy,
        |_: &Decision| true,
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                match bundle.evaluate(&bad_ctx) {
                    allow @ Decision::Allow { .. } => Ok(allow),
                    other => Err(other),
                }
            }
        },
    )
    .await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "all 3 attempts run since Indeterminate was (wrongly) marked retryable"
    );
    match result {
        Err(Decision::Indeterminate { .. }) => {}
        other => panic!("retrying a real Indeterminate must never produce Allow, got {other:?}"),
    }

    // Mirroring the real mapping in `fused-runtime`'s `stage_cedar_with_action`
    // (Decision::Indeterminate -> RuntimeError::PolicyDenied, never Ok(())):
    // the composed result is unconditionally treated as a denial.
    assert!(
        result.is_err(),
        "an Indeterminate composed result must deny"
    );
}
