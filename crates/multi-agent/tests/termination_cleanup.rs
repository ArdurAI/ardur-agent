//! §5.0 Phase 1 — termination removes a sub-agent from the registry: it no
//! longer appears in `list()`, and a second termination of the same handle is
//! rejected as already-terminated.

mod common;

use ardur_multi_agent::{MultiAgentError, MultiAgentRuntime, TerminationReason};
use common::{runtime_with, spec};

#[tokio::test]
async fn terminated_sub_agent_is_dropped_from_the_registry() {
    let (runtime, _parent_receipt_id, _root) = runtime_with(&["fs.read"], 10_000);

    let handle = runtime
        .spawn(spec("ephemeral-1", vec![], 1_000))
        .await
        .expect("spawn");

    // Live before termination.
    let listed = runtime.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].agent_id, handle.agent_id);

    runtime
        .terminate(handle.clone(), TerminationReason::Completed)
        .await
        .expect("terminate");

    // Gone after termination.
    assert!(runtime.list().is_empty());
    assert_eq!(runtime.cents_used(&handle.agent_id), None);

    // A second termination of the same id is rejected as already-terminated.
    let err = runtime
        .terminate(handle.clone(), TerminationReason::Completed)
        .await
        .expect_err("re-terminate is rejected");
    assert!(matches!(err, MultiAgentError::AlreadyTerminated(id) if id == handle.agent_id));
}
