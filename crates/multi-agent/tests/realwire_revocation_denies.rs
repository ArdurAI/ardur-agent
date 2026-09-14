//! gh#361 — a parent revokes a sub-agent's token and the sub-agent keeps going.
//!
//! Revocation is the only kill switch for a delegated capability: expiry is a
//! timer, attenuation is decided at spawn, and neither can stop a sub-agent
//! that is misbehaving *now*. If revoking does not deny, the time-boxed
//! exposure of a compromised delegation is its whole token lifetime rather
//! than "until someone notices".
//!
//! `CapVerifyingRuntime` builds its verifier over a fresh, empty
//! `HashSetDenyList` (`child.rs`), never shared with whoever holds the parent
//! token, so nothing a parent revokes is visible to the child.
//!
//! These tests are written before the fix and are expected to fail until a
//! shared deny list is threaded through.

mod common;

use ardur_multi_agent::{AttenuationRule, MultiAgentError, MultiAgentRuntime, RuntimeError};
use common::{ask, spec, verifying_runtime_with_deny};

/// Revoking the parent token denies a sub-agent spawned from it.
#[tokio::test]
async fn a_revoked_parent_token_denies_its_sub_agent() {
    let (runtime, _parent_receipt_id, _root, deny, parent_token) =
        verifying_runtime_with_deny(&["chat.submit", "fs.read"], 10_000);

    let handle = runtime
        .spawn(spec("worker-1", vec![], 10_000))
        .await
        .expect("spawn");

    // Works before revocation — otherwise the assertion below would pass for
    // the wrong reason (a sub-agent that never worked at all).
    runtime
        .ask(&handle, ask("do some work", 25))
        .await
        .expect("an un-revoked sub-agent runs");

    // The parent pulls the kill switch.
    deny.revoke_token(&parent_token);

    let err = runtime
        .ask(&handle, ask("keep going after revocation", 25))
        .await
        .expect_err("a revoked delegation must not keep executing");

    // Assert the VARIANT, not the debug text: a substring match passes for
    // `Internal` too, which is exactly the misclassification under review.
    match err {
        MultiAgentError::Runtime(RuntimeError::CapDenied { reason }) => {
            assert!(
                reason.to_lowercase().contains("revoked"),
                "expected a revocation reason, got: {reason}"
            );
        }
        other => panic!("expected RuntimeError::CapDenied, got {other:?}"),
    }
}

/// An attenuated child token is denied when the parent it derives from is
/// revoked.
///
/// Biscuit attenuation appends blocks to the parent token, so the child
/// carries the parent's revocation ids as well as its own. Revoking the parent
/// must therefore kill every delegation minted from it — otherwise revocation
/// is trivially evaded by delegating first.
#[tokio::test]
async fn revoking_a_parent_kills_delegations_already_minted_from_it() {
    let (runtime, _parent_receipt_id, _root, deny, parent_token) =
        verifying_runtime_with_deny(&["chat.submit", "fs.read"], 10_000);

    let handle = runtime
        .spawn(spec(
            "reader-1",
            vec![AttenuationRule::RestrictTools(vec![
                "chat.submit".to_string(),
            ])],
            10_000,
        ))
        .await
        .expect("spawn");

    runtime
        .ask(&handle, ask("work before revocation", 25))
        .await
        .expect("the attenuated sub-agent runs");

    deny.revoke_token(&parent_token);

    let err = runtime
        .ask(&handle, ask("work after revocation", 25))
        .await
        .expect_err("revoking the parent must kill delegations minted from it");

    // Assert the VARIANT, not the debug text: a substring match passes for
    // `Internal` too, which is exactly the misclassification under review.
    match err {
        MultiAgentError::Runtime(RuntimeError::CapDenied { reason }) => {
            assert!(
                reason.to_lowercase().contains("revoked"),
                "expected a revocation reason, got: {reason}"
            );
        }
        other => panic!("expected RuntimeError::CapDenied, got {other:?}"),
    }
}

/// Revocation does not leak across unrelated runtimes.
///
/// The contrast case. A deny list that denied everything — or one shared
/// globally by accident — would pass both tests above while breaking every
/// other delegation in the process.
#[tokio::test]
async fn revoking_one_parent_does_not_deny_an_unrelated_one() {
    let (runtime_a, _ra, _root_a, deny_a, token_a) =
        verifying_runtime_with_deny(&["chat.submit"], 10_000);
    let (runtime_b, _rb, _root_b, _deny_b, _token_b) =
        verifying_runtime_with_deny(&["chat.submit"], 10_000);

    let handle_a = runtime_a
        .spawn(spec("a-1", vec![], 10_000))
        .await
        .expect("spawn a");
    let handle_b = runtime_b
        .spawn(spec("b-1", vec![], 10_000))
        .await
        .expect("spawn b");

    deny_a.revoke_token(&token_a);

    runtime_a
        .ask(&handle_a, ask("should be denied", 25))
        .await
        .expect_err("a's delegation is revoked");

    runtime_b
        .ask(&handle_b, ask("should still work", 25))
        .await
        .expect("b is unaffected by a's revocation");
}
