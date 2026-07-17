//! §11.17: revoking a cap-token mid-session fires on_revoke with the
//! revocation reason.

mod support;

use std::sync::Arc;

use ardur_lifecycle_hooks::{HookEvent, HookRegistry, HookedRuntime, RecordingHook};
use ardur_runtime::{CapTokenRef, SessionId};

use support::{EchoProvider, test_model};

#[tokio::test]
async fn revoke_fires_on_revoke_with_reason() {
    let provider = Arc::new(EchoProvider::new());

    let recorder = Arc::new(RecordingHook::new("observer"));
    let log = recorder.event_log();

    let mut registry = HookRegistry::new();
    registry.register(recorder);

    let runtime = HookedRuntime::new(Arc::new(registry), provider, test_model());

    // Mint a cap, then revoke it mid-session.
    let cap = CapTokenRef("cap-to-revoke".to_string());
    let session_id = SessionId::new();

    let errors = runtime
        .revoke_cap_token(session_id, cap, "budget exhausted")
        .await;
    assert!(errors.is_empty(), "observer hook raised no errors");

    let events = log.lock().clone();
    let revoke = events
        .iter()
        .find(|e| matches!(e, HookEvent::OnRevoke { .. }))
        .expect("an OnRevoke event was recorded");

    match revoke {
        HookEvent::OnRevoke {
            session_id: sid,
            revocation_reason,
            ..
        } => {
            assert_eq!(*sid, session_id);
            assert_eq!(revocation_reason, "budget exhausted");
        }
        other => panic!("unexpected event: {other:?}"),
    }
}
