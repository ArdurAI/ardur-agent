//! §5.1 E2E — the full attenuation chain through the real wire.
//!
//! `e2e-tests` is not yet on `dev`, so this scenario lives inline in the
//! multi-agent crate (per the §5.1 plan). It exercises the whole substrate path:
//! a parent runtime mints authority, spawns a sub-agent under a narrowed
//! cap-token, the sub-agent runs a real submit through the verifying child
//! runtime, and the termination receipt links back into the parent's audit
//! anchor. The escalation-denial half then shows a sub-agent narrowed *past*
//! `chat.submit` cannot run at all.

mod common;

use ardur_multi_agent::{AttenuationRule, MultiAgentError, MultiAgentRuntime, TerminationReason};
use common::{ask, spec, verifying_runtime_with};

#[tokio::test]
async fn full_chain_spawn_authorized_submit_then_terminate_links_parent() {
    let (runtime, parent_receipt_id, _root) =
        verifying_runtime_with(&["chat.submit", "fs.read"], 10_000);

    // --- Spawn a sub-agent under a narrowed cap (still holds chat.submit). ---
    let handle = runtime
        .spawn(spec(
            "delegate-1",
            vec![AttenuationRule::RestrictTools(vec![
                "chat.submit".to_string(),
            ])],
            2_000,
        ))
        .await
        .expect("spawn");

    // Its authority is strictly narrower than the parent's — the narrowing block
    // shows up as an extra revocation id beyond the parent's single one.
    let attenuated = runtime
        .attenuated_token(&handle.agent_id)
        .expect("attenuated token");
    assert!(
        attenuated.revocation_ids().len() >= 2,
        "child cap fingerprint must differ from the parent's"
    );

    // --- The sub-agent runs a real, authorized submit through the wire. ---
    let resp = runtime
        .ask(&handle, ask("summarize the findings", 150))
        .await
        .expect("authorized child submit");
    assert_eq!(resp.message.content, "summarize the findings");
    assert!(
        resp.sub_receipts.is_empty(),
        "no nested receipts in Phase 1"
    );
    // The cost was charged against the sub-agent's envelope.
    assert_eq!(resp.cost_used.cents, 150);
    assert_eq!(runtime.cents_used(&handle.agent_id), Some(150));

    // --- Terminate: the receipt links back into the parent's audit chain. ---
    let receipt = runtime
        .terminate(handle, TerminationReason::Completed)
        .await
        .expect("terminate");
    assert_eq!(receipt.reason, TerminationReason::Completed);
    assert_eq!(receipt.agent_id.0, "delegate-1");
    assert_eq!(receipt.total_cost.cents, 150);
    assert_eq!(
        receipt.parent_receipt_id, parent_receipt_id,
        "termination receipt must link the parent's anchor"
    );
}

#[tokio::test]
async fn full_chain_escalation_denied_no_spend() {
    // A sub-agent narrowed to a tool set that excludes chat.submit cannot run a
    // turn at all — the verifying wire refuses to authorize it.
    let (runtime, _parent_receipt_id, _root) =
        verifying_runtime_with(&["chat.submit", "fs.read"], 10_000);

    let handle = runtime
        .spawn(spec(
            "overreach-1",
            vec![AttenuationRule::RestrictTools(vec!["fs.read".to_string()])],
            2_000,
        ))
        .await
        .expect("spawn");

    let err = runtime
        .ask(&handle, ask("act beyond my grant", 150))
        .await
        .expect_err("a sub-agent without chat.submit is denied");
    assert!(
        matches!(err, MultiAgentError::Runtime(_)),
        "expected a runtime denial, got {err:?}"
    );

    // Denied turns spend nothing.
    assert_eq!(runtime.cents_used(&handle.agent_id), Some(0));

    // The sub-agent can still be torn down cleanly, charging zero.
    let receipt = runtime
        .terminate(handle, TerminationReason::Completed)
        .await
        .expect("terminate");
    assert_eq!(receipt.total_cost.cents, 0);
}
