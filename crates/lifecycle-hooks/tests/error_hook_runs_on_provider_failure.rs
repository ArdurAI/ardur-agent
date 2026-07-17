//! §11.17: when the provider errors, on_error fires with phase = Provider.

mod support;

use std::sync::Arc;

use ardur_lifecycle_hooks::{
    HookEvent, HookRegistry, HookedRuntime, LifecyclePhase, RecordingHook,
};
use ardur_runtime::{ChatRuntime, RuntimeError};

use support::{ErroringProvider, test_model, user_request};

#[tokio::test]
async fn provider_failure_fires_on_error_with_provider_phase() {
    let provider = Arc::new(ErroringProvider::new());

    let recorder = Arc::new(RecordingHook::new("observer"));
    let log = recorder.event_log();

    let mut registry = HookRegistry::new();
    registry.register(recorder);

    let runtime = HookedRuntime::new(Arc::new(registry), provider.clone(), test_model());

    let err = runtime
        .submit(user_request("hello", "cap-1"))
        .await
        .expect_err("a failing provider fails the turn");
    assert!(matches!(err, RuntimeError::ProviderUnavailable));

    // The provider was reached (the failure is at the provider phase, not
    // admission)...
    assert_eq!(provider.call_count(), 1);

    // ...and on_error fired with phase = Provider.
    let events = log.lock().clone();
    let saw_provider_error = events.iter().any(|e| {
        matches!(
            e,
            HookEvent::OnError {
                phase: LifecyclePhase::Provider,
                ..
            }
        )
    });
    assert!(
        saw_provider_error,
        "expected an OnError(Provider) event, got {events:?}"
    );

    // No receipt was minted, so no post-receipt observation.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, HookEvent::OnPostReceipt { .. }))
    );
}
