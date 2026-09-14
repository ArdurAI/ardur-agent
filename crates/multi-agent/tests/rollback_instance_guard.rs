//! gh#367 review — a rollback must reach only the instance that reserved.
//!
//! Agent ids are reusable: `terminate` removes the entry, and a later `spawn`
//! with the same id installs a *new* `SubAgent` with a fresh meter. The ask
//! error path rolls a reservation back by looking the agent up in the
//! registry, so without an instance check that rollback lands on whichever
//! agent currently holds the id.
//!
//! Saturating arithmetic alone does not save this. It stops the counter
//! wrapping to ~4 billion, but a stale release against a respawned agent
//! silently erases that agent's own legitimate usage, and later asks can then
//! exceed its lifetime envelope. Under-counting is the quieter failure of the
//! two, which is what makes it worth a test.

mod common;

use ardur_multi_agent::{AttenuationRule, MultiAgentRuntime, TerminationReason};
use common::{ask, spec, verifying_runtime_with};

/// A stale rollback must not erase a respawned agent's usage.
#[tokio::test]
async fn a_rollback_does_not_reach_a_respawned_agent() {
    let (runtime, _rid, _root) = verifying_runtime_with(&["chat.submit"], 10_000);

    // The first instance: attenuated so its ask is DENIED at the substrate,
    // which is the path that rolls the reservation back.
    let first = runtime
        .spawn(spec(
            "reused-id",
            vec![AttenuationRule::RestrictTools(vec!["fs.read".to_string()])],
            10_000,
        ))
        .await
        .expect("spawn first");

    // Terminate it, freeing the id, then respawn the SAME id with authority
    // that works. The new instance has its own fresh meter.
    runtime
        .terminate(first.clone(), TerminationReason::Completed)
        .await
        .expect("terminate first");

    let second = runtime
        .spawn(spec("reused-id", vec![], 10_000))
        .await
        .expect("respawn the same id");

    // The new instance does real work and legitimately consumes budget.
    runtime
        .ask(&second, ask("real work", 400))
        .await
        .expect("the respawned agent runs");
    let after_work = runtime.cents_used(&second.agent_id);
    assert_eq!(
        after_work,
        Some(400),
        "the respawned agent has legitimately used 400c"
    );

    // Now drive a failing ask through the STALE handle. Its reservation
    // rollback must not touch the respawned agent's meter.
    let _ = runtime.ask(&first, ask("stale ask", 400)).await;

    assert_eq!(
        runtime.cents_used(&second.agent_id),
        Some(400),
        "a rollback for a terminated instance must not erase the respawned \
         agent's usage — otherwise later asks exceed its envelope"
    );
}
