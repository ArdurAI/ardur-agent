use ardur_hooks_openclaw_compat::{
    AdapterKind, CanonicalHookEventName, CanonicalHookFirePayload, CodexResponseShape,
    DefaultOpenClawMigrationTranslator, DefaultOpenClawPayloadSerializer,
    DefaultOpenClawResponseParser, HookResponseEnvelope, MigrationWarningReason,
    OpenClawCodexEventName, OpenClawHookConfig, OpenClawHookEntry, OpenClawHookEventNameMap,
    OpenClawHookMeta, OpenClawMigrationTranslator, OpenClawPayloadSerializer,
    OpenClawResponseParser, OpenClawResponseWarning, PermissionBehavior,
};
use serde_json::json;

#[test]
fn event_name_map_uses_frozen_codex_to_canonical_table() {
    assert_eq!(
        OpenClawHookEventNameMap::to_canonical(OpenClawCodexEventName::PreToolUse),
        CanonicalHookEventName::PreToolCall
    );
    assert_eq!(
        OpenClawHookEventNameMap::to_canonical(OpenClawCodexEventName::PostToolUse),
        CanonicalHookEventName::PostToolCall
    );
    assert_eq!(
        OpenClawHookEventNameMap::to_canonical(OpenClawCodexEventName::PermissionRequest),
        CanonicalHookEventName::PreApprovalRequest
    );
    assert_eq!(
        OpenClawHookEventNameMap::to_canonical(OpenClawCodexEventName::BeforeAgentFinalize),
        CanonicalHookEventName::SubagentStop
    );
}

#[test]
fn payload_serializer_emits_codex_shape() {
    let serializer = DefaultOpenClawPayloadSerializer;
    let mut payload = CanonicalHookFirePayload::new(
        CanonicalHookEventName::PreToolCall,
        "sess-1",
        json!({"command": "git push --force"}),
        "2026-06-15T12:00:00Z",
    );
    payload.cwd = Some("/work/repo".into());
    payload.tool_name = Some("shell".to_string());

    let mut meta = OpenClawHookMeta::new("relay-1", "run-1");
    meta.tool_use_id = Some("tool-1".to_string());

    let bytes = serializer.serialize(&payload, &meta).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(value["provider"], "codex");
    assert_eq!(value["relayId"], "relay-1");
    assert_eq!(value["event"], "pre_tool_use");
    assert_eq!(value["nativeEventName"], "PreToolUse");
    assert_eq!(value["sessionId"], "sess-1");
    assert_eq!(value["runId"], "run-1");
    assert_eq!(value["cwd"], "/work/repo");
    assert_eq!(value["toolName"], "shell");
    assert_eq!(value["toolUseId"], "tool-1");
    assert_eq!(value["rawPayload"]["command"], "git push --force");
    assert_eq!(value["receivedAt"], "2026-06-15T12:00:00Z");
}

#[test]
fn response_parser_blocks_pre_tool_use_denial() {
    let parser = DefaultOpenClawResponseParser;
    let parsed = parser
        .parse(
            br#"{
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": "force-push blocked"
                }
            }"#,
        )
        .unwrap();

    assert_eq!(
        parsed.envelope,
        HookResponseEnvelope::Block {
            reason: "force-push blocked".to_string()
        }
    );
    assert_eq!(
        parsed.source_shape,
        Some(CodexResponseShape::HookSpecificOutputPreToolUse)
    );
    assert_eq!(parsed.warning, None);
}

#[test]
fn response_parser_blocks_before_agent_finalize_shapes() {
    let parser = DefaultOpenClawResponseParser;

    let revise = parser
        .parse(br#"{"decision":"block","reason":"revise final answer"}"#)
        .unwrap();
    assert_eq!(
        revise.envelope,
        HookResponseEnvelope::Block {
            reason: "revise final answer".to_string()
        }
    );
    assert_eq!(
        revise.source_shape,
        Some(CodexResponseShape::BeforeAgentFinalizeRevise)
    );

    let stop = parser
        .parse(br#"{"continue":false,"stopReason":"stop now"}"#)
        .unwrap();
    assert_eq!(
        stop.envelope,
        HookResponseEnvelope::Block {
            reason: "stop now".to_string()
        }
    );
    assert_eq!(
        stop.source_shape,
        Some(CodexResponseShape::BeforeAgentFinalizeStop)
    );
}

#[test]
fn response_parser_downgrades_permission_request_deny() {
    let parser = DefaultOpenClawResponseParser;
    let parsed = parser
        .parse(
            br#"{
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {
                        "behavior": "deny",
                        "message": "do not allow"
                    }
                }
            }"#,
        )
        .unwrap();

    assert_eq!(parsed.envelope, HookResponseEnvelope::NoOp);
    assert_eq!(
        parsed.source_shape,
        Some(CodexResponseShape::HookSpecificOutputPermissionRequest)
    );
    assert_eq!(
        parsed.permission_decision.unwrap().behavior,
        PermissionBehavior::Deny
    );
    assert_eq!(
        parsed.warning,
        Some(OpenClawResponseWarning::PermissionDecisionDowngraded {
            behavior: PermissionBehavior::Deny,
            message: Some("do not allow".to_string())
        })
    );
}

#[test]
fn response_parser_downgrades_permission_request_allow() {
    let parser = DefaultOpenClawResponseParser;
    let parsed = parser
        .parse(
            br#"{
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {
                        "behavior": "allow"
                    }
                }
            }"#,
        )
        .unwrap();

    assert_eq!(parsed.envelope, HookResponseEnvelope::NoOp);
    assert_eq!(
        parsed.permission_decision.unwrap().behavior,
        PermissionBehavior::Allow
    );
    assert_eq!(
        parsed.warning,
        Some(OpenClawResponseWarning::PermissionDecisionDowngraded {
            behavior: PermissionBehavior::Allow,
            message: None
        })
    );
}

#[test]
fn response_parser_treats_empty_and_unknown_as_noop() {
    let parser = DefaultOpenClawResponseParser;

    let empty = parser.parse(b"  \n").unwrap();
    assert_eq!(empty.envelope, HookResponseEnvelope::NoOp);
    assert_eq!(empty.warning, None);

    let unknown = parser.parse(br#"{"hello":"world"}"#).unwrap();
    assert_eq!(unknown.envelope, HookResponseEnvelope::NoOp);
    assert_eq!(unknown.warning, Some(OpenClawResponseWarning::UnknownShape));

    let unknown_permission_behavior = parser
        .parse(
            br#"{
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {
                        "behavior": "maybe"
                    }
                }
            }"#,
        )
        .unwrap();
    assert_eq!(
        unknown_permission_behavior.envelope,
        HookResponseEnvelope::NoOp
    );
    assert_eq!(
        unknown_permission_behavior.warning,
        Some(OpenClawResponseWarning::UnknownShape)
    );
}

#[test]
fn migration_translator_marks_openclaw_format_and_warnings() {
    let translator = DefaultOpenClawMigrationTranslator;
    let config = OpenClawHookConfig {
        hooks: vec![
            OpenClawHookEntry {
                event: OpenClawCodexEventName::PreToolUse,
                command: "check-force-push".to_string(),
                matcher: Some("shell".to_string()),
                allowed_events: vec![OpenClawCodexEventName::PreToolUse],
                timeout_ms: Some(500),
                provider: Some("codex".to_string()),
            },
            OpenClawHookEntry {
                event: OpenClawCodexEventName::PermissionRequest,
                command: "audit-permission".to_string(),
                matcher: None,
                allowed_events: vec![OpenClawCodexEventName::PermissionRequest],
                timeout_ms: None,
                provider: Some("other".to_string()),
            },
        ],
        source_path: Some("/tmp/openclaw.json".into()),
        source_format: None,
    };

    let report = translator.translate(&config).unwrap();

    assert_eq!(report.entries.len(), 2);
    assert_eq!(report.entries[0].adapter_kind, AdapterKind::OpenClaw);
    assert_eq!(report.entries[0].format, "openclaw");
    assert_eq!(
        report.entries[0].canonical_event,
        CanonicalHookEventName::PreToolCall
    );
    assert_eq!(report.entries[0].migration_completeness, 100);
    assert_eq!(
        report.entries[1].canonical_event,
        CanonicalHookEventName::PreApprovalRequest
    );
    assert_eq!(report.entries[1].migration_completeness, 0);
    assert!(report.warnings.iter().any(|warning| {
        matches!(
            warning.reason,
            MigrationWarningReason::PermissionVetoDowngraded { .. }
        )
    }));
    assert!(report.warnings.iter().any(|warning| {
        matches!(
            &warning.reason,
            MigrationWarningReason::UnsupportedProvider { provider }
                if provider == "other"
        )
    }));
}
