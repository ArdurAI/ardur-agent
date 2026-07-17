use ardur_media_audio::{
    AudioInput, AudioModelId, AudioProviderId, AudioScope, AudioVerb, AuthorizedAudioToken,
    ContentClass, DiarizationMode, MediaProvider, MissionId, TranscribeFileRequest,
    TranscriptFormat, TranscriptionProvider, UnixTsMillis, VoiceTranscribeTool, WhisperApiConfig,
    WhisperApiTranscriptionProvider,
};
use ardur_media_decode::AudioFormat;
use ardur_runtime::{CapTokenRef, SessionId};
use ardur_tool_registry::{Tool, ToolContext};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tiny_wav() -> Vec<u8> {
    // Minimal RIFF/WAVE-like bytes; the provider unit path treats bytes opaquely
    // and the mock upstream stands in for Whisper's decoder.
    b"RIFF$\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0@\x1f\0\0@\x1f\0\0\x01\0\x08\0data\0\0\0\0".to_vec()
}

#[tokio::test]
async fn whisper_base_url_with_path_preserves_v1_segment() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .and(header("authorization", "Bearer test-openai-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "text": "hello from v1 whisper",
            "language": "en"
        })))
        .mount(&mock)
        .await;

    let config = WhisperApiConfig::new("test-openai-key")
        .with_base_url(format!("{}/v1", mock.uri()))
        .expect("loopback mock base URL with path is accepted");
    let provider = WhisperApiTranscriptionProvider::new(config).expect("provider config is valid");
    let provider_id = provider.media_provider_id().clone();

    let transcript = provider
        .transcribe_file(
            &token(provider_id.clone()),
            request(provider_id, AudioModelId::new("whisper-1"), tiny_wav()),
        )
        .await
        .expect("nested /v1 endpoint is called");

    assert_eq!(transcript.segments[0].text, "hello from v1 whisper");
}

fn token(provider_id: AudioProviderId) -> AuthorizedAudioToken {
    AuthorizedAudioToken {
        cap_token: CapTokenRef("cap-token-for-voice.transcribe".to_string()),
        scope: AudioScope {
            verb: AudioVerb::TranscribeFile,
            provider_id,
            duration_seconds_ceiling: 60,
            content_class_ceiling: ContentClass::Safe,
        },
    }
}

fn request(
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
        mission_id: MissionId::new("mission.voice-test"),
        requested_at: UnixTsMillis(1),
    }
}

#[tokio::test]
async fn whisper_api_provider_transcribes_inline_audio_and_records_receipt_hash() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .and(header("authorization", "Bearer test-openai-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "text": "hello from whisper",
            "language": "en"
        })))
        .mount(&mock)
        .await;

    let config = WhisperApiConfig::new("test-openai-key")
        .with_base_url(mock.uri())
        .expect("loopback mock base URL is accepted")
        .with_model("whisper-1");
    let provider = WhisperApiTranscriptionProvider::new(config).expect("provider config is valid");
    let provider_id = provider.media_provider_id().clone();
    let model_id = AudioModelId::new("whisper-1");

    let transcript = provider
        .transcribe_file(
            &token(provider_id.clone()),
            request(provider_id.clone(), model_id.clone(), tiny_wav()),
        )
        .await
        .expect("mock whisper transcription succeeds");

    assert_eq!(transcript.provider_id, provider_id);
    assert_eq!(transcript.model_id, model_id);
    assert_eq!(transcript.segments.len(), 1);
    assert_eq!(transcript.segments[0].text, "hello from whisper");
    assert_eq!(transcript.language_detected.as_str(), "en");
    assert!(
        !transcript.receipt_hash.as_str().is_empty(),
        "provider operation is chained into a receipt hash"
    );
}

#[tokio::test]
async fn whisper_api_provider_rejects_invalid_empty_audio_before_network() {
    let mock = MockServer::start().await;
    let config = WhisperApiConfig::new("test-openai-key")
        .with_base_url(mock.uri())
        .expect("loopback mock base URL is accepted");
    let provider = WhisperApiTranscriptionProvider::new(config).expect("provider config is valid");
    let provider_id = provider.media_provider_id().clone();

    let err = provider
        .transcribe_file(
            &token(provider_id.clone()),
            request(provider_id, AudioModelId::new("whisper-1"), Vec::new()),
        )
        .await
        .expect_err("empty audio is invalid");

    assert!(err.to_string().contains("audio bytes must be non-empty"));
    assert_eq!(
        mock.received_requests()
            .await
            .expect("requests queried")
            .len(),
        0
    );
}

#[tokio::test]
async fn voice_transcribe_tool_requires_a_non_empty_cap_token() {
    let mock = MockServer::start().await;
    let config = WhisperApiConfig::new("test-openai-key")
        .with_base_url(mock.uri())
        .expect("loopback mock base URL is accepted");
    let provider = WhisperApiTranscriptionProvider::new(config).expect("provider config is valid");
    let tool = VoiceTranscribeTool::new(provider);
    let ctx = ToolContext {
        cap_token: CapTokenRef(String::new()),
        session_id: SessionId::new(),
        invocation_id: Default::default(),
        cwd: std::env::current_dir().expect("cwd"),
        env: Default::default(),
        cost_budget_cents: 100,
    };

    let err = tool
        .invoke(
            &ctx,
            json!({
                "audio_base64": BASE64_STANDARD.encode(tiny_wav()),
                "format": "wav",
                "duration_seconds_upper_bound": 3
            }),
        )
        .await
        .expect_err("empty cap token is denied before provider call");

    assert!(err.to_string().contains("cap-token"));
}

#[tokio::test]
async fn voice_transcribe_tool_rejects_audio_duration_above_local_ceiling() {
    let mock = MockServer::start().await;
    let config = WhisperApiConfig::new("test-openai-key")
        .with_base_url(mock.uri())
        .expect("loopback mock base URL is accepted");
    let provider = WhisperApiTranscriptionProvider::new(config).expect("provider config is valid");
    let tool = VoiceTranscribeTool::new(provider);
    let ctx = ToolContext {
        cap_token: CapTokenRef("cap-token-for-voice.transcribe".to_string()),
        session_id: SessionId::new(),
        invocation_id: Default::default(),
        cwd: std::env::current_dir().expect("cwd"),
        env: Default::default(),
        cost_budget_cents: 100,
    };

    let err = tool
        .invoke(
            &ctx,
            json!({
                "audio_base64": BASE64_STANDARD.encode(tiny_wav()),
                "format": "wav",
                "duration_seconds_upper_bound": 7_201
            }),
        )
        .await
        .expect_err("duration above the tool ceiling is denied before provider call");

    assert!(
        err.to_string().contains("duration_seconds_upper_bound")
            || err.to_string().contains("duration")
    );
    assert_eq!(
        mock.received_requests()
            .await
            .expect("requests queried")
            .len(),
        0
    );
}
