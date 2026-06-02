//! §11.17: hooks fire in ascending priority order (lower runs first),
//! regardless of registration order.

mod support;

use std::sync::Arc;

use ardur_lifecycle_hooks::{EventLog, HookEvent, HookRegistry, HookedRuntime, RecordingHook};
use ardur_runtime::ChatRuntime;
use parking_lot::Mutex;

use support::{EchoProvider, test_model, user_request};

#[tokio::test]
async fn lower_priority_runs_first() {
    let provider = Arc::new(EchoProvider::new());

    // One shared log; two recorders. Register the high-priority one FIRST to
    // prove ordering is by priority, not registration order.
    let log: EventLog = Arc::new(Mutex::new(Vec::new()));
    let high = Arc::new(RecordingHook::with_shared_log("hook.late", 10, log.clone()));
    let low = Arc::new(RecordingHook::with_shared_log(
        "hook.early",
        -10,
        log.clone(),
    ));

    let mut registry = HookRegistry::new();
    registry.register(high);
    registry.register(low);

    let runtime = HookedRuntime::new(Arc::new(registry), provider, test_model());
    runtime
        .submit(user_request("hello", "cap-1"))
        .await
        .expect("turn succeeds");

    // The pre-submit observations, in the order they fired.
    let pre_submit_order: Vec<String> = log
        .lock()
        .iter()
        .filter_map(|e| match e {
            HookEvent::OnPreSubmit { hook_id, .. } => Some(hook_id.to_string()),
            _ => None,
        })
        .collect();

    assert_eq!(
        pre_submit_order,
        vec!["hook.early".to_string(), "hook.late".to_string()],
        "priority -10 must run before priority 10"
    );
}
