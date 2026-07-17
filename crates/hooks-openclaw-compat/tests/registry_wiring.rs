use std::sync::Arc;

use ardur_hooks_openclaw_compat::{
    OpenClawCodexEventName, OpenClawHookConfig, OpenClawHookEntry, OpenClawHookRegistryExt,
    RecordingOpenClawRunner,
};
use ardur_lifecycle_hooks::{HookRegistry, PreSubmitCtx, PreSubmitOutcome};
use ardur_provider_runtime::{CompletionRequest, ModelId};
use ardur_runtime::{CapTokenRef, ChatMessage, SessionId};

fn openclaw_entry(command: &str) -> OpenClawHookEntry {
    OpenClawHookEntry {
        event: OpenClawCodexEventName::PreToolUse,
        command: command.to_string(),
        matcher: None,
        allowed_events: vec![OpenClawCodexEventName::PreToolUse],
        timeout_ms: Some(250),
        provider: Some("codex".to_string()),
    }
}

#[tokio::test]
async fn openclaw_hooks_register_with_lifecycle_registry_and_fire_in_order() {
    let runner = Arc::new(RecordingOpenClawRunner::new());
    let config = OpenClawHookConfig {
        hooks: vec![openclaw_entry("first"), openclaw_entry("second")],
        source_path: None,
        source_format: None,
    };
    let mut registry = HookRegistry::new();
    let report = registry
        .register_openclaw_hooks_with_runner(&config, runner.clone())
        .expect("OpenClaw hooks register");

    assert_eq!(report.entries.len(), 2);
    assert_eq!(registry.len(), 2);

    let request = CompletionRequest::new(
        vec![ChatMessage::user("please run a tool")],
        ModelId::new("stub-model"),
        16,
    );
    let cap = CapTokenRef("cap-token".to_string());
    let ctx = PreSubmitCtx {
        session_id: SessionId::new(),
        request: &request,
        cap_token_id: &cap,
        attempt: 1,
    };

    assert!(matches!(
        registry.run_pre_submit(&ctx).await,
        PreSubmitOutcome::Continue
    ));
    assert_eq!(
        runner.fired_commands(),
        vec![
            "pre_tool_use:first".to_string(),
            "pre_tool_use:second".to_string()
        ]
    );
}

#[test]
fn openclaw_hook_registration_rejects_malformed_entries() {
    let runner = Arc::new(RecordingOpenClawRunner::new());
    let config = OpenClawHookConfig {
        hooks: vec![openclaw_entry("   ")],
        source_path: None,
        source_format: None,
    };
    let mut registry = HookRegistry::new();

    let err = registry
        .register_openclaw_hooks_with_runner(&config, runner)
        .expect_err("missing command is rejected");
    assert!(err.to_string().contains("missing command"));
    assert!(registry.is_empty());
}
