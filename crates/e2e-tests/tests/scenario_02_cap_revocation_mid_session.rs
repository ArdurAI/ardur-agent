//! Scenario §2.E2 — `cap_revocation_mid_session`.
//!
//! Drives the *fused* [`ChatRuntime`] (cap-token → cedar → cost-gate → hooks →
//! provider → receipt → memory → journal) across a capability revocation that
//! happens *between* two turns of one session:
//!
//! 1. Mint a valid cap-token and submit a turn — it succeeds end to end.
//! 2. Revoke that token mid-session via
//!    [`FusedRuntime::revoke_cap_token`](ardur_fused_runtime::FusedRuntime::revoke_cap_token).
//! 3. Submit a second turn carrying the *same* token — the runtime rejects it at
//!    stage 1 with [`RuntimeError::CapDenied`], and the provider is never reached
//!    a second time.
//!
//! Unlike the per-crate cap-token tests (which verify a token in isolation), this
//! proves the revocation is honoured *on the real submit path*, ahead of every
//! downstream stage.

use std::sync::Arc;

use ardur_e2e_tests::fixtures::{self};

use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, RuntimeError, SessionId, SubmitRequest,
};

mod support;
use support::EchoProvider;

#[tokio::test]
async fn revocation_mid_session_denies_subsequent_turns() {
    let provider = Arc::new(EchoProvider::new());
    let runtime = fixtures::fused_builder(provider.clone())
        .build()
        .expect("the fused runtime wires");

    let token = fixtures::dev_valid_cap_token();
    let session_id = SessionId::new();

    let request = |content: &str| SubmitRequest {
        messages: vec![ChatMessage::user(content)],
        cap_token: CapTokenRef(token.clone()),
        session_id,
        requested_provider: None,
    };

    // ---- 1. First turn succeeds through the whole substrate.
    let first = runtime
        .submit(request("before revocation"))
        .await
        .expect("the pre-revocation turn completes");
    assert_eq!(first.response.content, "before revocation");
    assert_eq!(
        provider.call_count(),
        1,
        "the first turn reached the provider"
    );

    // ---- 2. Revoke the token mid-session.
    runtime
        .revoke_cap_token(
            session_id,
            CapTokenRef(token.clone()),
            "operator revoked the session token",
        )
        .await
        .expect("the revoke is accepted");

    // ---- 3. The same token is now denied at stage 1 — before any further
    //         provider dispatch.
    let err = runtime
        .submit(request("after revocation"))
        .await
        .expect_err("a revoked token must be denied");
    assert!(
        matches!(err, RuntimeError::CapDenied { .. }),
        "expected CapDenied, got {err:?}"
    );
    assert_eq!(
        provider.call_count(),
        1,
        "the post-revocation turn never reached the provider"
    );
}
