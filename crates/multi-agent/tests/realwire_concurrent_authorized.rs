//! §5.1 real wire — concurrency safety. Many sub-agents asking at once each go
//! through the verifying child runtime; all authorized turns complete and each
//! envelope reflects only its own spend. This guards against the verifying path
//! introducing lock contention or a shared-state race the §5.0 echo path didn't
//! have.

mod common;

use ardur_multi_agent::{AttenuationRule, MultiAgentRuntime};
use common::{ask, spec, verifying_runtime_with};
use futures::future::join_all;

#[tokio::test]
async fn four_verified_sub_agents_ask_concurrently() {
    let (runtime, _parent_receipt_id, _root) =
        verifying_runtime_with(&["chat.submit", "fs.read"], 100_000);

    // Each sub-agent is narrowed to chat.submit (still authorized) with its own
    // 1_000c envelope.
    let mut handles = Vec::new();
    for i in 0..4 {
        let handle = runtime
            .spawn(spec(
                &format!("verified-{i}"),
                vec![AttenuationRule::RestrictTools(vec![
                    "chat.submit".to_string(),
                ])],
                1_000,
            ))
            .await
            .expect("spawn");
        handles.push(handle);
    }

    // Two concurrent turns each: 8 authorized asks in flight at once.
    let mut futures = Vec::new();
    for (i, handle) in handles.iter().enumerate() {
        for turn in 0..2 {
            futures.push(runtime.ask(handle, ask(&format!("agent {i} turn {turn}"), 30)));
        }
    }
    let responses = join_all(futures).await;

    assert_eq!(responses.len(), 8);
    for (n, resp) in responses.into_iter().enumerate() {
        resp.unwrap_or_else(|e| panic!("authorized ask {n} failed: {e:?}"));
    }

    // Each sub-agent spent exactly its own two 30c turns — no leakage.
    for handle in &handles {
        assert_eq!(runtime.cents_used(&handle.agent_id), Some(60));
    }
}
