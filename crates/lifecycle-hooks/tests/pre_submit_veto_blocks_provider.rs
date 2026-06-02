//! §11.17: a pre-submit hook that vetoes blocks the turn before the provider
//! is ever called.

mod support;

use std::sync::Arc;

use ardur_lifecycle_hooks::{HookRegistry, HookedRuntime};
use ardur_runtime::{ChatRuntime, RuntimeError};

use support::{EchoProvider, VetoHook, test_model, user_request};

#[tokio::test]
async fn veto_blocks_provider_and_surfaces_vetoed_by_hook() {
    let provider = Arc::new(EchoProvider::new());

    let mut registry = HookRegistry::new();
    registry.register(Arc::new(VetoHook::new(
        "policy.deny",
        0,
        "blocked by policy",
    )));

    let runtime = HookedRuntime::new(Arc::new(registry), provider.clone(), test_model());

    let err = runtime
        .submit(user_request("hello", "cap-1"))
        .await
        .expect_err("a vetoing hook must block the turn");

    match err {
        RuntimeError::VetoedByHook { hook_id, reason } => {
            assert_eq!(hook_id, "policy.deny");
            assert_eq!(reason, "blocked by policy");
        }
        other => panic!("expected VetoedByHook, got {other:?}"),
    }

    // The whole point: the provider was never reached.
    assert_eq!(
        provider.call_count(),
        0,
        "provider must not be called when a pre-submit hook vetoes"
    );
}
