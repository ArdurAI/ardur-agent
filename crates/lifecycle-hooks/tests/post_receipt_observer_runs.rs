//! §11.17: a RecordingHook observes exactly one post-receipt event for a
//! successful turn, carrying the right session id.

mod support;

use std::sync::Arc;

use ardur_lifecycle_hooks::{HookEvent, HookRegistry, HookedRuntime, RecordingHook};
use ardur_runtime::ChatRuntime;

use support::{EchoProvider, test_model, user_request};

#[tokio::test]
async fn post_receipt_observer_fires_once_with_session_id() {
    let provider = Arc::new(EchoProvider::new());

    let recorder = Arc::new(RecordingHook::new("observer"));
    let log = recorder.event_log();

    let mut registry = HookRegistry::new();
    registry.register(recorder);

    let runtime = HookedRuntime::new(Arc::new(registry), provider, test_model());

    let req = user_request("hello", "cap-1");
    let session_id = req.session_id;

    runtime.submit(req).await.expect("turn succeeds");

    let events = log.lock().clone();
    let post_receipts: Vec<&HookEvent> = events
        .iter()
        .filter(|e| matches!(e, HookEvent::OnPostReceipt { .. }))
        .collect();

    assert_eq!(
        post_receipts.len(),
        1,
        "exactly one post-receipt event expected, got {events:?}"
    );
    match post_receipts[0] {
        HookEvent::OnPostReceipt {
            session_id: sid, ..
        } => assert_eq!(*sid, session_id),
        other => panic!("unexpected event: {other:?}"),
    }

    // A successful turn also recorded a pre-submit observation, and never an
    // error.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HookEvent::OnPreSubmit { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, HookEvent::OnError { .. }))
    );
}
