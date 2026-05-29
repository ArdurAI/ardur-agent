//! §5.0 Phase 1 — many sub-agents run concurrently with isolated budgets.
//! Spawn five sub-agents, ask each two turns concurrently, and assert all ten
//! responses arrive and each sub-agent's meter reflects only its own spend.

mod common;

use ardur_multi_agent::MultiAgentRuntime;
use common::{ask, runtime_with, spec};
use futures::future::join_all;

#[tokio::test]
async fn five_sub_agents_ask_concurrently_with_isolated_budgets() {
    let (runtime, _parent_receipt_id, _root) = runtime_with(&["fs.read"], 100_000);

    // Spawn five sub-agents, each with its own 1_000c envelope.
    let mut handles = Vec::new();
    for i in 0..5 {
        let handle = runtime
            .spawn(spec(&format!("agent-{i}"), vec![], 1_000))
            .await
            .expect("spawn");
        handles.push(handle);
    }

    // Two concurrent turns per sub-agent: 10 asks in flight at once.
    let mut futures = Vec::new();
    for (i, handle) in handles.iter().enumerate() {
        for turn in 0..2 {
            futures.push(runtime.ask(handle, ask(&format!("agent {i} turn {turn}"), 20)));
        }
    }
    let responses = join_all(futures).await;

    // All ten turns succeeded.
    assert_eq!(responses.len(), 10);
    for (n, resp) in responses.into_iter().enumerate() {
        resp.unwrap_or_else(|e| panic!("ask {n} failed: {e:?}"));
    }

    // Each sub-agent spent exactly its own two turns (2 × 20c), nothing leaked
    // across budgets.
    for handle in &handles {
        assert_eq!(runtime.cents_used(&handle.agent_id), Some(40));
    }
}
