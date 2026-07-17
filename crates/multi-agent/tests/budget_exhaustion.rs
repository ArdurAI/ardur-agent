//! §5.0 Phase 1 — the budget envelope is a hard ceiling: an ask that would push
//! a sub-agent past its envelope is rejected *before* the child runs, and the
//! parent records the exhaustion as the termination reason.

mod common;

use ardur_multi_agent::{MultiAgentError, MultiAgentRuntime, TerminationReason};
use common::{ask, runtime_with, spec};

#[tokio::test]
async fn over_envelope_ask_is_rejected_before_submit() {
    let (runtime, _parent_receipt_id, _root) = runtime_with(&["fs.read"], 10_000);

    // A 500c lifetime envelope.
    let handle = runtime
        .spawn(spec("worker-1", vec![], 500))
        .await
        .expect("spawn");

    // First ask reserves 300c — fits.
    runtime
        .ask(&handle, ask("first", 300))
        .await
        .expect("first ask fits");
    assert_eq!(runtime.cents_used(&handle.agent_id), Some(300));

    // Second ask would need another 300c (600 > 500) — rejected up front.
    let err = runtime
        .ask(&handle, ask("second", 300))
        .await
        .expect_err("second ask exceeds the envelope");

    match err {
        MultiAgentError::BudgetExhausted {
            agent,
            used,
            envelope,
        } => {
            assert_eq!(agent.0, "worker-1");
            assert_eq!(used, 300);
            assert_eq!(envelope, 500);
        }
        other => panic!("expected BudgetExhausted, got {other:?}"),
    }

    // The rejected ask consumed nothing further — the meter is unchanged.
    assert_eq!(runtime.cents_used(&handle.agent_id), Some(300));

    // The parent tears the sub-agent down, recording the cause.
    let receipt = runtime
        .terminate(handle, TerminationReason::BudgetExhausted)
        .await
        .expect("terminate");
    assert_eq!(receipt.reason, TerminationReason::BudgetExhausted);
    assert_eq!(receipt.total_cost.cents, 300);
}
