//! Scenario §2.E7 — `cedar_deny_default`.
//!
//! Proves Cedar's **deny-by-default** posture *through the full fused runtime*,
//! not by calling [`evaluate`] in isolation. Every subcase builds a fresh
//! [`FusedRuntime`] that differs only in its Cedar bundle, then submits the
//! same request with the same permissive cap-token, so the policy decision is
//! the sole cause of the turn's outcome. The runtime evaluates the bundle at
//! stage 2 (after the cap-token verifies, before any budget is reserved) and
//! folds every non-`Allow` decision onto [`RuntimeError::PolicyDenied`].
//!
//! The four subcases walk Cedar's three decision variants and the runtime's
//! mapping of each:
//!
//! | subcase | bundle vs. the request                    | `Decision`      | submit outcome            |
//! |---------|-------------------------------------------|-----------------|---------------------------|
//! | 1       | a `permit` that matches                   | `Allow`         | `Ok` (stub completion)    |
//! | 2       | a `permit` **and** a matching `forbid`    | `Deny`          | `Err(PolicyDenied)`       |
//! | 3       | a `permit` that does **not** match, no `forbid` | `Deny` (implicit) | `Err(PolicyDenied)` |
//! | 4       | a `permit` guarded by a missing attribute | `Indeterminate` | `Err(PolicyDenied)`       |
//!
//! ## What Cedar's decision enum actually looks like
//!
//! [`ardur_cedar_policy::Decision`] is three-state: `Allow`, `Deny`, and
//! `Indeterminate`. The naming in the §2.E7 brief ("Indeterminate falls through
//! to Deny") anticipates a two-state engine where a non-match is reported as
//! Indeterminate. Cedar is not that engine: a request that satisfies **no**
//! `permit` and trips **no** `forbid` is a native *implicit* `Deny`
//! (`matched_policy_ids: []`), **not** `Indeterminate`. `Indeterminate` is
//! reserved for genuine *evaluation errors* — e.g. a `when` clause reading an
//! attribute the resource does not carry. So the security-critical "no policy
//! matched ⇒ deny" property (subcase 3) is proven via the `Deny` arm, while the
//! distinct `Indeterminate` arm is exercised separately (subcase 4).
//!
//! ## How [`FusedRuntime`] maps each variant (stage 2)
//!
//! - `Decision::Allow` → the pipeline proceeds to the cost gate.
//! - `Decision::Deny { reason, .. }` → `RuntimeError::PolicyDenied { reason }`
//!   (the deciding engine's reason, verbatim).
//! - `Decision::Indeterminate { reason }` →
//!   `RuntimeError::PolicyDenied { reason: format!("indeterminate: {reason}") }`.
//!
//! The two error arms are therefore distinguishable *through the runtime* by the
//! `PolicyDenied` reason: an `indeterminate:`-prefixed reason came from the
//! `Indeterminate` arm; any other from the `Deny` arm. The subcases assert that
//! prefix to prove all three Cedar variants are routed correctly — and, for
//! subcase 3, that a non-matching request is **not** silently treated as
//! `Allow`.
//!
//! [`evaluate`]: ardur_cedar_policy::PolicyBundle::evaluate
//! [`FusedRuntime`]: ardur_fused_runtime::FusedRuntime

use std::sync::Arc;

use ardur_e2e_tests::fixtures::{self};

use ardur_cedar_policy::{CedarPolicyBundle, PolicyBundle, PolicySource};
use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, RuntimeError, SessionId, SubmitRequest, SubmitResult,
};

/// Compile an embedded Cedar bundle for one subcase. A parse failure is a test
/// authoring bug, so it panics rather than threading an error.
fn bundle(policy: &str) -> CedarPolicyBundle {
    CedarPolicyBundle::load(PolicySource::Embedded(policy.to_string()))
        .expect("the scenario policy compiles")
}

/// The Cedar principal the runtime derives for these subcases: `User::"<subject>"`
/// where the subject is the holder [`fixtures::dev_valid_cap_token`] is minted for
/// ([`fixtures::TEST_HOLDER`]). Kept in lock-step with the fixture by the
/// `debug_assert` in [`explicit_permit_allows_the_turn`].
const PRINCIPAL: &str = r#"User::"spiffe://ardur/user/e2e""#;

/// Build a fresh fused runtime over `policy` and submit one fixed turn through
/// it. Everything but the Cedar bundle is the shared, permissive fixture: a
/// valid well-funded cap-token, the stub provider, the manual clock, and a
/// generous budget. So the returned outcome is a function of the policy alone.
///
/// The runtime *derives* the Cedar query from the verified cap-token: the
/// principal is [`PRINCIPAL`] (`User::"<subject>"`, the subject
/// [`fixtures::dev_valid_cap_token`] mints), the action is the builder default
/// `Action::Submit`, and the resource is `Session::"<session id>"` carrying the
/// cap claims as attributes. Every policy below is written against exactly that
/// derived request.
///
/// [`FusedRuntimeBuilder`]: ardur_fused_runtime::FusedRuntimeBuilder
async fn submit_under(policy: &str) -> Result<SubmitResult, RuntimeError> {
    let runtime =
        fixtures::fused_builder_with_policies(Arc::new(fixtures::stub_provider()), bundle(policy))
            .build()
            .expect("the fused runtime wires");

    runtime
        .submit(SubmitRequest {
            messages: vec![ChatMessage::user("authorize this turn")],
            cap_token: CapTokenRef(fixtures::dev_valid_cap_token()),
            session_id: SessionId::new(),
            requested_provider: None,
        })
        .await
}

/// Subcase 1 — an explicit `permit` matching the request authorizes the turn,
/// so the submit runs to completion and returns the stub provider's output. The
/// authorization seam said `Allow`; nothing downstream blocks it.
#[tokio::test]
async fn explicit_permit_allows_the_turn() {
    // Guard: PRINCIPAL must stay in lock-step with the cap-token the fixture
    // mints — the runtime derives the principal id from that subject.
    debug_assert_eq!(
        PRINCIPAL,
        format!("User::{:?}", fixtures::TEST_HOLDER),
        "the derived principal must match the fixture's cap-token subject"
    );

    let outcome = submit_under(&format!(
        r#"permit (principal == {PRINCIPAL}, action == Action::"Submit", resource);"#,
    ))
    .await
    .expect("a matching permit authorizes the turn");

    assert_eq!(
        outcome.response.content, "[anthropic stub]",
        "the authorized turn reached the provider and returned its completion"
    );
}

/// Subcase 2 — an explicit `forbid` that matches the request wins over a
/// matching `permit` (Cedar's forbid-overrides-permit rule), so the decision is
/// `Deny` and the submit fails with [`RuntimeError::PolicyDenied`] before the
/// provider is reached. The reason is the engine's, *not* the `indeterminate:`
/// fold, proving the `Deny` arm — not the `Indeterminate` arm — fired.
#[tokio::test]
async fn explicit_forbid_overrides_permit_and_denies() {
    let err = submit_under(&format!(
        "permit (principal, action, resource);\n\
         forbid (principal == {PRINCIPAL}, action == Action::\"Submit\", resource);",
    ))
    .await
    .expect_err("a matching forbid denies the turn");

    match err {
        RuntimeError::PolicyDenied { reason } => {
            assert!(
                !reason.starts_with("indeterminate:"),
                "an explicit forbid is a Deny, not an Indeterminate fold: {reason:?}"
            );
        }
        other => panic!("expected PolicyDenied for an explicit forbid, got {other:?}"),
    }
}

/// Subcase 3 — **the security-critical property.** The bundle carries a single
/// `permit` that does *not* match the request (a different principal) and no
/// `forbid` at all. No `permit` is satisfied, so Cedar returns an *implicit*
/// `Deny` — the request is **not** silently treated as `Allow` just because no
/// rule spoke to it. The runtime surfaces that as [`RuntimeError::PolicyDenied`]
/// before the provider is reached. Deny-by-default holds.
#[tokio::test]
async fn no_matching_policy_falls_through_to_deny() {
    let err = submit_under(r#"permit (principal == User::"nobody", action, resource);"#)
        .await
        .expect_err("a request that matches no permit is denied by default");

    match err {
        RuntimeError::PolicyDenied { reason } => {
            // Implicit deny travels through the `Deny` arm, not the
            // `Indeterminate` fold — the absence of any matching rule is a
            // decision, not an evaluation error.
            assert!(
                !reason.starts_with("indeterminate:"),
                "an unmatched request is an implicit Deny, not Indeterminate: {reason:?}"
            );
        }
        other => panic!(
            "deny-by-default must surface as PolicyDenied — a non-matching request \
             was NOT authorized; got {other:?}"
        ),
    }
}

/// Subcase 4 (bonus) — a genuine `Indeterminate`. The lone `permit` is guarded
/// by `when { resource.tier == "free" }`. The derived resource *does* carry the
/// cap-claim attributes (`audience`, `tools`, `expires_unix`, …) but no `tier`,
/// so evaluating `resource.tier` is an *error*, not a non-match. Cedar reports
/// `Indeterminate`, which the runtime folds to
/// [`RuntimeError::PolicyDenied`] with an `indeterminate:`-prefixed reason — the
/// fold direction the brief calls out: an indeterminate evaluation never yields
/// `Allow`.
#[tokio::test]
async fn indeterminate_evaluation_falls_through_to_deny() {
    let err =
        submit_under(r#"permit (principal, action, resource) when { resource.tier == "free" };"#)
            .await
            .expect_err("an indeterminate evaluation denies the turn");

    match err {
        RuntimeError::PolicyDenied { reason } => {
            assert!(
                reason.starts_with("indeterminate:"),
                "an evaluation error must travel through the Indeterminate fold: {reason:?}"
            );
        }
        other => panic!("expected PolicyDenied for an Indeterminate decision, got {other:?}"),
    }
}
