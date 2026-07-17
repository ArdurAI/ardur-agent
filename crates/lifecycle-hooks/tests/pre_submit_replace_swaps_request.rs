//! §11.17: a pre-submit hook that replaces the request body causes the provider
//! to receive the replacement, not the original.

mod support;

use std::sync::Arc;

use ardur_lifecycle_hooks::{HookRegistry, HookedRuntime};
use ardur_runtime::{ChatRuntime, Role};

use support::{EchoProvider, ReplaceHook, test_model, user_request};

#[tokio::test]
async fn replace_swaps_the_request_the_provider_sees() {
    let provider = Arc::new(EchoProvider::new());

    let mut registry = HookRegistry::new();
    registry.register(Arc::new(ReplaceHook::new("augment", 0, "replaced-by-hook")));

    let runtime = HookedRuntime::new(Arc::new(registry), provider.clone(), test_model());

    let result = runtime
        .submit(user_request("original prompt", "cap-1"))
        .await
        .expect("replace is not a veto — the turn proceeds");

    // The provider was called exactly once, with the replaced body.
    assert_eq!(provider.call_count(), 1);
    let seen = provider.last_request().expect("provider saw a request");
    assert_eq!(seen.messages.len(), 1);
    assert_eq!(seen.messages[0].role, Role::User);
    assert_eq!(seen.messages[0].content, "replaced-by-hook");

    // And the echoed response reflects the replacement, not the original.
    assert_eq!(result.response.content, "replaced-by-hook");
}
