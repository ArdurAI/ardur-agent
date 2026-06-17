//! EPIC-TOOLS cross-crate integration: media-audio, ACP, automation, and
//! OpenClaw hook compatibility wired together with positive and negative paths.

use std::sync::Arc;

use ardur_acp::{ACP_METHOD_SESSION_PROMPT, AcpMessage, AcpRequest, AcpWireCodec};
use ardur_automation::{
    BundleHash, DefaultTaskFlowOrchestrator, FlowControl, FlowNode, FlowStep, Sha256Digest,
    StepDispatch, StepFailureKind, StepId, StepOutcome, TaskCreationRequest, TaskFlowDag,
    TaskFlowOrchestrator, ToolId,
};
use ardur_cap_token::{HolderId, VerifiedClaims};
use ardur_hooks_openclaw_compat::{
    OpenClawCodexEventName, OpenClawHookConfig, OpenClawHookEntry, OpenClawHookRegistryExt,
    RecordingOpenClawRunner,
};
use ardur_lifecycle_hooks::{HookRegistry, PreSubmitCtx, PreSubmitOutcome};
use ardur_media_audio::{
    AudioInput, AudioModelId, AudioProviderId, AudioScope, AudioVerb, AuthorizedAudioToken,
    ContentClass, DiarizationMode, MediaProvider, MissionId, TranscribeFileRequest,
    TranscriptFormat, TranscriptionProvider, WhisperApiConfig, WhisperApiTranscriptionProvider,
};
use ardur_media_decode::AudioFormat;
use ardur_provider_runtime::{CompletionRequest, ModelId};
use ardur_runtime::{CapTokenRef, ChatMessage, SessionId};
use serde_json::json;
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tiny_wav() -> Vec<u8> {
    b"RIFF$\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0@\x1f\0\0@\x1f\0\0\x01\0\x08\0data\0\0\0\0".to_vec()
}

fn verified_claims() -> VerifiedClaims {
    VerifiedClaims {
        token_id: Uuid::now_v7(),
        audience: "ardur-epic-tools".to_string(),
        subject: HolderId("spiffe://tenant/user".to_string()),
        expires_unix: 4_102_444_800,
        budget_remaining: 1_000,
        tool_allowlist: vec!["task.create".to_string()],
    }
}

fn audio_token(provider_id: AudioProviderId) -> AuthorizedAudioToken {
    AuthorizedAudioToken {
        cap_token: CapTokenRef("cap-token-for-epic-tools".to_string()),
        scope: AudioScope {
            verb: AudioVerb::TranscribeFile,
            provider_id,
            duration_seconds_ceiling: 60,
            content_class_ceiling: ContentClass::Safe,
        },
    }
}

fn transcribe_request(
    provider_id: AudioProviderId,
    model_id: AudioModelId,
    bytes: Vec<u8>,
) -> TranscribeFileRequest {
    TranscribeFileRequest {
        provider_id,
        model_id,
        input: AudioInput::InlineBytes {
            bytes,
            format: AudioFormat::Wav,
        },
        duration_seconds_upper_bound: 3,
        language_hint: None,
        target_language: None,
        diarization: DiarizationMode::None,
        max_speakers: None,
        export_format: TranscriptFormat::Json,
        mission_id: MissionId::new("mission.epic-tools"),
        requested_at: 1,
    }
}

fn step(name: &str, estimated_duration_ms: u32) -> FlowStep {
    FlowStep {
        step_id: StepId::new(),
        step_name: name.to_string(),
        dispatch: StepDispatch::ToolCall {
            tool_id: ToolId("tool.echo".to_string()),
            args_template: json!({ "message": name }),
        },
        verify_via: None,
        estimated_cost_micro_usd: 10,
        estimated_duration_ms,
    }
}

fn dag_from_steps(steps: Vec<FlowStep>) -> TaskFlowDag {
    TaskFlowDag {
        dag_hash: BundleHash(Sha256Digest([42; 32])),
        version: 1,
        root: FlowNode::Control(FlowControl::Sequence(
            steps.into_iter().map(FlowNode::Step).collect(),
        )),
        max_depth: 4,
        max_fanout: 4,
        invariants: Vec::new(),
        estimated_total_cost_micro_usd: 20,
    }
}

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
async fn transcribe_then_orchestrate_emit_acp_and_fire_openclaw_hooks() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .and(header("authorization", "Bearer test-openai-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "text": "summarize the incident",
            "language": "en"
        })))
        .mount(&mock)
        .await;

    let provider = WhisperApiTranscriptionProvider::new(
        WhisperApiConfig::new("test-openai-key")
            .with_base_url(mock.uri())
            .expect("loopback mock base URL is accepted"),
    )
    .expect("provider config is valid");
    let provider_id = provider.media_provider_id().clone();
    let transcript = provider
        .transcribe_file(
            &audio_token(provider_id.clone()),
            transcribe_request(provider_id, AudioModelId::new("whisper-1"), tiny_wav()),
        )
        .await
        .expect("transcription succeeds");
    let transcript_text = transcript.segments[0].text.clone();
    assert_eq!(transcript_text, "summarize the incident");
    assert!(!transcript.receipt_hash.as_str().is_empty());

    let orchestrator = DefaultTaskFlowOrchestrator::new();
    let handle = orchestrator
        .create_task(
            &verified_claims(),
            TaskCreationRequest {
                description: transcript_text.clone(),
                flow_dag: Some(dag_from_steps(vec![step("plan", 5), step("execute", 5)])),
            },
        )
        .await
        .expect("DAG executes");
    let state = orchestrator
        .get_task_state(handle.task_id)
        .await
        .expect("state is queryable");
    assert!(matches!(
        state.outcome,
        Some(ardur_automation::TaskOutcome::Succeeded)
    ));

    let acp = AcpMessage::Request(AcpRequest::new(
        "req-epic-tools",
        ACP_METHOD_SESSION_PROMPT,
        Some(json!({ "prompt": transcript_text, "taskId": handle.task_id })),
    ));
    let frame = AcpWireCodec::encode_message(&acp).expect("ACP encodes");
    assert_eq!(AcpWireCodec::decode_line(&frame).expect("ACP decodes"), acp);

    let runner = Arc::new(RecordingOpenClawRunner::new());
    let config = OpenClawHookConfig {
        hooks: vec![
            openclaw_entry("audit-first"),
            openclaw_entry("audit-second"),
        ],
        source_path: None,
        source_format: None,
    };
    let mut registry = HookRegistry::new();
    registry
        .register_openclaw_hooks_with_runner(&config, runner.clone())
        .expect("OpenClaw hooks register");
    let request = CompletionRequest::new(
        vec![ChatMessage::user("please call the transcription workflow")],
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
            "pre_tool_use:audit-first".to_string(),
            "pre_tool_use:audit-second".to_string()
        ]
    );
}

#[tokio::test]
async fn invalid_audio_and_timeout_paths_are_observable() {
    let mock = MockServer::start().await;
    let provider = WhisperApiTranscriptionProvider::new(
        WhisperApiConfig::new("test-openai-key")
            .with_base_url(mock.uri())
            .expect("loopback mock base URL is accepted"),
    )
    .expect("provider config is valid");
    let provider_id = provider.media_provider_id().clone();
    let err = provider
        .transcribe_file(
            &audio_token(provider_id.clone()),
            transcribe_request(provider_id, AudioModelId::new("whisper-1"), Vec::new()),
        )
        .await
        .expect_err("empty audio is rejected before network");
    assert!(err.to_string().contains("audio bytes must be non-empty"));
    assert_eq!(
        mock.received_requests()
            .await
            .expect("requests queried")
            .len(),
        0
    );

    let orchestrator = DefaultTaskFlowOrchestrator::new().with_step_timeout_ms(10);
    let slow_step = step("slow", 25);
    let slow_step_id = slow_step.step_id;
    let handle = orchestrator
        .create_task(
            &verified_claims(),
            TaskCreationRequest {
                description: "timeout path".to_string(),
                flow_dag: Some(dag_from_steps(vec![slow_step])),
            },
        )
        .await
        .expect("timeout is recorded in state");
    let state = orchestrator
        .get_task_state(handle.task_id)
        .await
        .expect("state is queryable");
    assert!(matches!(
        state.outcome,
        Some(ardur_automation::TaskOutcome::Failed)
    ));
    assert!(matches!(
        state.step_outcomes.get(&slow_step_id),
        Some(StepOutcome::Failed {
            failure_kind: StepFailureKind::TimeoutExceeded
        })
    ));
}
