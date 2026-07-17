//! §5.1 real wire — the expiry axis flows through. A sub-agent whose token was
//! attenuated to an expiry already in the past is denied at submit: the
//! verifying child runtime evaluates the token against the *current* wall-clock
//! second, so a stale delegation cannot keep running.

mod common;

use ardur_multi_agent::{AttenuationRule, MultiAgentError, MultiAgentRuntime, RuntimeError};
use common::{ask, spec, verifying_runtime_with};

#[tokio::test]
async fn sub_agent_with_a_past_expiry_is_denied() {
    let (runtime, _parent_receipt_id, _root) = verifying_runtime_with(&["chat.submit"], 10_000);

    // Bring the expiry back to one second after the epoch — comfortably in the
    // past relative to any real wall-clock now.
    let handle = runtime
        .spawn(spec(
            "expired-1",
            vec![AttenuationRule::EarlierExpiry(1)],
            10_000,
        ))
        .await
        .expect("spawn");

    let err = runtime
        .ask(&handle, ask("are you still there", 10))
        .await
        .expect_err("an expired sub-agent token cannot authorize a turn");

    // Expiry maps to the dedicated §1.0 variant, not the Internal catch-all.
    assert!(
        matches!(err, MultiAgentError::Runtime(RuntimeError::CapTokenExpired)),
        "expected CapTokenExpired, got {err:?}"
    );

    assert_eq!(runtime.cents_used(&handle.agent_id), Some(0));
}
