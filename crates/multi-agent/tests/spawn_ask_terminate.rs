//! §5.0 Phase 1 — the happy path: spawn one sub-agent, send three turns, then
//! terminate it. Every turn yields a receipt, and the termination receipt links
//! back to the parent's audit anchor.

mod common;

use ardur_multi_agent::{MultiAgentRuntime, TerminationReason};
use common::{ask, runtime_with, spec};

#[tokio::test]
async fn spawn_three_turns_then_terminate_completed() {
    let (runtime, parent_receipt_id, _root) = runtime_with(&["fs.read", "fs.write"], 10_000);

    let handle = runtime
        .spawn(spec("researcher-1", vec![], 10_000))
        .await
        .expect("spawn");

    // Three turns, each reserving a little budget; each must produce a receipt.
    let mut receipts = Vec::new();
    for i in 0..3 {
        let resp = runtime
            .ask(&handle, ask(&format!("step {i}"), 10))
            .await
            .expect("ask");
        // The echo runtime returns the user content back as the assistant reply.
        assert_eq!(resp.message.content, format!("step {i}"));
        assert!(resp.sub_receipts.is_empty());
        receipts.push(resp.receipt_id);
    }

    // Distinct receipts per turn.
    assert_eq!(receipts.len(), 3);
    assert_ne!(receipts[0], receipts[1]);
    assert_ne!(receipts[1], receipts[2]);

    // Three turns of 10c were reserved.
    assert_eq!(runtime.cents_used(&handle.agent_id), Some(30));

    let receipt = runtime
        .terminate(handle, TerminationReason::Completed)
        .await
        .expect("terminate");

    assert_eq!(receipt.reason, TerminationReason::Completed);
    assert_eq!(receipt.agent_id.0, "researcher-1");
    assert_eq!(receipt.total_cost.cents, 30);
    // The termination receipt links back into the parent's chain.
    assert_eq!(receipt.parent_receipt_id, parent_receipt_id);
}
