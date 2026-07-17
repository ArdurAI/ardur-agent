//! §5.1 real wire — the teeth. A sub-agent whose tool allowlist was narrowed to
//! drop `chat.submit` can no longer run a chat turn: the verifying child runtime
//! refuses to authorize the attenuated token, the ask fails, and the failed
//! reservation is rolled back off the envelope. Under the §5.0 echo child this
//! same ask *would have succeeded* (see `realwire_echo_does_not_gate`), so this
//! is the behavioral change the real wire buys.

mod common;

use ardur_multi_agent::{AttenuationRule, MultiAgentError, MultiAgentRuntime, RuntimeError};
use common::{ask, spec, verifying_runtime_with};

#[tokio::test]
async fn sub_agent_without_chat_submit_is_denied_at_the_substrate() {
    // Parent may chat and read; the sub-agent is narrowed to fs.read only —
    // chat.submit is attenuated away.
    let (runtime, _parent_receipt_id, _root) =
        verifying_runtime_with(&["chat.submit", "fs.read"], 10_000);

    let handle = runtime
        .spawn(spec(
            "reader-1",
            vec![AttenuationRule::RestrictTools(vec!["fs.read".to_string()])],
            10_000,
        ))
        .await
        .expect("spawn");

    // The turn never gets authorized: chat.submit is no longer in the token.
    let err = runtime
        .ask(&handle, ask("try to chat anyway", 25))
        .await
        .expect_err("a sub-agent without chat.submit cannot run a chat turn");

    // The denial surfaces as a child-runtime error carrying the cap-token
    // reason (§1.0 has no typed `Unauthorized`, so it rides `Internal`).
    match err {
        MultiAgentError::Runtime(RuntimeError::Internal(inner)) => {
            let msg = inner.to_string();
            assert!(
                msg.contains("tool not in cap-token allowlist"),
                "expected a tool-allowlist denial, got: {msg}"
            );
        }
        other => panic!("expected a runtime denial, got {other:?}"),
    }

    // The failed ask charged nothing: the reservation was rolled back.
    assert_eq!(runtime.cents_used(&handle.agent_id), Some(0));
}
