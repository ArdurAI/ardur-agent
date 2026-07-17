//! §11.17: when two hooks both veto, the lower-priority (first-firing) one's
//! reason is the one that surfaces — first veto wins, short-circuiting the rest.

mod support;

use std::sync::Arc;

use ardur_lifecycle_hooks::{HookRegistry, HookedRuntime};
use ardur_runtime::{ChatRuntime, RuntimeError};

use support::{EchoProvider, VetoHook, test_model, user_request};

#[tokio::test]
async fn first_veto_in_priority_order_wins() {
    let provider = Arc::new(EchoProvider::new());

    let mut registry = HookRegistry::new();
    // Register the higher-priority (later) one first to prove the winner is
    // chosen by priority, not registration order.
    registry.register(Arc::new(VetoHook::new("veto.late", 10, "second reason")));
    registry.register(Arc::new(VetoHook::new("veto.early", -10, "first reason")));

    let runtime = HookedRuntime::new(Arc::new(registry), provider.clone(), test_model());

    let err = runtime
        .submit(user_request("hello", "cap-1"))
        .await
        .expect_err("a vetoing hook blocks the turn");

    match err {
        RuntimeError::VetoedByHook { hook_id, reason } => {
            assert_eq!(
                hook_id, "veto.early",
                "the first (lowest-priority) veto wins"
            );
            assert_eq!(reason, "first reason");
        }
        other => panic!("expected VetoedByHook, got {other:?}"),
    }

    assert_eq!(provider.call_count(), 0);
}
