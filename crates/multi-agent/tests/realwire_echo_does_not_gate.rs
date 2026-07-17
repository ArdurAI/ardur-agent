//! §5.1 real wire — the contrast that justifies the wire. The §5.0 echo child
//! (`InMemoryMultiAgentRuntime::in_memory`) trusts any non-empty cap-token, so a
//! sub-agent attenuated *away* from `chat.submit` still runs its turn. The
//! verifying wire (see `realwire_attenuated_tool_denied`) denies that exact
//! spawn. This test pins the pre-wire behavior so the difference is explicit and
//! regression-guarded.

mod common;

use ardur_multi_agent::{AttenuationRule, MultiAgentRuntime};
use common::{ask, runtime_with, spec};

#[tokio::test]
async fn echo_child_runs_a_turn_even_without_chat_submit() {
    // Echo runtime, parent grants both tools.
    let (runtime, _parent_receipt_id, _root) = runtime_with(&["chat.submit", "fs.read"], 10_000);

    // Sub-agent narrowed to fs.read only — chat.submit attenuated away.
    let handle = runtime
        .spawn(spec(
            "ungated-1",
            vec![AttenuationRule::RestrictTools(vec!["fs.read".to_string()])],
            10_000,
        ))
        .await
        .expect("spawn");

    // The echo child never authorizes the token, so the turn succeeds despite
    // the missing capability. This is precisely the gap §5.1 closes.
    let resp = runtime
        .ask(&handle, ask("echo me", 10))
        .await
        .expect("echo child does not gate on the attenuated cap");
    assert_eq!(resp.message.content, "echo me");
    assert_eq!(runtime.cents_used(&handle.agent_id), Some(10));
}
