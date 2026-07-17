//! §5.1 real wire — the audience axis flows through too. A sub-agent pinned to a
//! different audience than the one the runtime serves is denied at submit, even
//! though its tool allowlist still contains `chat.submit`. This proves the wire
//! authorizes the *whole* attenuated scope, not just the tool set.

mod common;

use ardur_multi_agent::{AttenuationRule, MultiAgentError, MultiAgentRuntime, RuntimeError};
use common::{ask, spec, verifying_runtime_with};

#[tokio::test]
async fn sub_agent_pinned_to_a_foreign_audience_is_denied() {
    // Runtime serves the "agent" audience (the parent token's audience).
    let (runtime, _parent_receipt_id, _root) = verifying_runtime_with(&["chat.submit"], 10_000);

    // Narrow the sub-agent's audience to something the runtime does not serve.
    let handle = runtime
        .spawn(spec(
            "stray-1",
            vec![AttenuationRule::RestrictAudience(
                "some-other-service".to_string(),
            )],
            10_000,
        ))
        .await
        .expect("spawn");

    let err = runtime
        .ask(&handle, ask("hello", 10))
        .await
        .expect_err("a foreign-audience sub-agent is not authorized here");

    match err {
        MultiAgentError::Runtime(RuntimeError::Internal(inner)) => {
            let msg = inner.to_string();
            assert!(
                msg.contains("audience mismatch"),
                "expected an audience-mismatch denial, got: {msg}"
            );
        }
        other => panic!("expected a runtime denial, got {other:?}"),
    }

    assert_eq!(runtime.cents_used(&handle.agent_id), Some(0));
}
