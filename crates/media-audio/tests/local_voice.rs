use std::fs;
use std::path::{Path, PathBuf};

use ardur_media_audio::{
    AudioInput, AudioModelId, AudioProviderId, AudioScope, AudioVerb, AuthorizedAudioToken,
    ContentClass, DiarizationMode, LocalSpeechToTextProvider, LocalSttConfig,
    LocalTextToSpeechProvider, LocalTtsConfig, MediaProvider, MissionId, TextToSpeechProvider,
    TextToSpeechRequest, TranscribeFileRequest, TranscriptFormat, TranscriptionProvider,
    VoiceSpeakTool,
};
use ardur_media_decode::AudioFormat;
use ardur_runtime::{CapTokenRef, SessionId};
use ardur_tool_registry::{Capability, Tool, ToolContext};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde_json::json;

fn tiny_wav() -> Vec<u8> {
    b"RIFF$\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0@\x1f\0\0@\x1f\0\0\x01\0\x08\0data\0\0\0\0".to_vec()
}

fn token(provider_id: AudioProviderId) -> AuthorizedAudioToken {
    AuthorizedAudioToken {
        cap_token: CapTokenRef("cap-token-for-local-voice".to_string()),
        scope: AudioScope {
            verb: AudioVerb::TranscribeFile,
            provider_id,
            duration_seconds_ceiling: 60,
            content_class_ceiling: ContentClass::Safe,
        },
    }
}

fn request(provider_id: AudioProviderId, model_id: AudioModelId) -> TranscribeFileRequest {
    TranscribeFileRequest {
        provider_id,
        model_id,
        input: AudioInput::InlineBytes {
            bytes: tiny_wav(),
            format: AudioFormat::Wav,
        },
        duration_seconds_upper_bound: 3,
        language_hint: None,
        target_language: None,
        diarization: DiarizationMode::None,
        max_speakers: None,
        export_format: TranscriptFormat::Json,
        mission_id: MissionId::new("mission.local-voice-test"),
        requested_at: 1,
    }
}

fn write_executable_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).expect("script written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("script executable");
    }
    path
}

#[tokio::test]
async fn local_stt_provider_invokes_configured_command_and_transcribes_audio() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script = write_executable_script(
        temp.path(),
        "local-stt.sh",
        "#!/bin/sh\ntest -s \"$1\" || exit 7\nprintf 'local transcript from device\\n'\n",
    );
    let provider = LocalSpeechToTextProvider::new(LocalSttConfig::new(script))
        .expect("local provider config is valid");
    let provider_id = provider.media_provider_id().clone();
    let model_id = AudioModelId::new("local-test-model");

    let transcript = provider
        .transcribe_file(
            &token(provider_id.clone()),
            request(provider_id, model_id.clone()),
        )
        .await
        .expect("local STT succeeds");

    assert_eq!(transcript.model_id, model_id);
    assert_eq!(transcript.segments[0].text, "local transcript from device");
    assert_eq!(transcript.language_detected.as_str(), "und");
    assert!(!transcript.receipt_hash.as_str().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_stt_transcriptions_compose_concurrently() {
    // M0: two transcriptions driven with `join!` both complete. The provider is
    // genuinely async now (the external command + temp-file I/O go through
    // `tokio::process`/`tokio::fs`), so they compose concurrently rather than
    // each parking a runtime worker for the whole command duration.
    let temp = tempfile::tempdir().expect("tempdir");
    let script = write_executable_script(
        temp.path(),
        "local-stt.sh",
        "#!/bin/sh\ntest -s \"$1\" || exit 7\nprintf 'concurrent transcript\\n'\n",
    );
    let provider = LocalSpeechToTextProvider::new(LocalSttConfig::new(script))
        .expect("local provider config is valid");
    let provider_id = provider.media_provider_id().clone();
    let model_id = AudioModelId::new("local-test-model");

    // Bind the tokens to locals so they outlive the joined futures that borrow them.
    let token_a = token(provider_id.clone());
    let token_b = token(provider_id.clone());
    let (first, second) = tokio::join!(
        provider.transcribe_file(&token_a, request(provider_id.clone(), model_id.clone())),
        provider.transcribe_file(&token_b, request(provider_id.clone(), model_id.clone())),
    );
    assert_eq!(
        first.expect("first transcription").segments[0].text,
        "concurrent transcript"
    );
    assert_eq!(
        second.expect("second transcription").segments[0].text,
        "concurrent transcript"
    );
}

#[tokio::test]
async fn local_tts_provider_uses_stdout_audio_and_records_receipt_hash() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script = write_executable_script(
        temp.path(),
        "local-tts.sh",
        "#!/bin/sh\nprintf 'WAV:%s' \"$1\"\n",
    );
    let provider = LocalTextToSpeechProvider::new(LocalTtsConfig::new(script))
        .expect("local TTS config is valid");
    let output = provider
        .synthesize(TextToSpeechRequest {
            text: "read approval aloud".to_string(),
            voice: Some("default".to_string()),
            format: AudioFormat::Wav,
            model_id: provider.default_model_id().clone(),
            mission_id: MissionId::new("mission.local-tts-test"),
            requested_at: 1,
        })
        .await
        .expect("local TTS succeeds");

    assert_eq!(output.provider_id, *provider.media_provider_id());
    assert_eq!(output.format, AudioFormat::Wav);
    assert_eq!(output.audio_bytes, b"WAV:read approval aloud");
    assert!(!output.receipt_hash.as_str().is_empty());
}

#[tokio::test]
async fn voice_speak_tool_requires_voice_output_and_process_capabilities() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script =
        write_executable_script(temp.path(), "local-tts.sh", "#!/bin/sh\nprintf 'AUDIO'\n");
    let provider = LocalTextToSpeechProvider::new(LocalTtsConfig::new(script))
        .expect("local TTS config is valid");
    let tool = VoiceSpeakTool::new(provider);

    let caps = tool.required_capabilities();
    assert!(caps.contains(&Capability::VoiceOutput));
    assert!(caps.contains(&Capability::ProcessSpawn));

    let ctx = ToolContext {
        cap_token: CapTokenRef("cap-token-for-voice.speak".to_string()),
        session_id: SessionId::new(),
        invocation_id: Default::default(),
        cwd: std::env::current_dir().expect("cwd"),
        env: Default::default(),
        cost_budget_cents: 100,
    };
    let result = tool
        .invoke(&ctx, json!({ "text": "hello", "format": "wav" }))
        .await
        .expect("voice.speak emits audio");
    assert_eq!(
        result.content["audio_base64"],
        BASE64_STANDARD.encode(b"AUDIO")
    );
    assert_eq!(result.content["format"], "wav");
}

#[tokio::test]
async fn local_tts_rejects_empty_text_before_spawning() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script = write_executable_script(temp.path(), "local-tts.sh", "#!/bin/sh\nexit 99\n");
    let provider = LocalTextToSpeechProvider::new(LocalTtsConfig::new(script))
        .expect("local TTS config is valid");

    let err = provider
        .synthesize(TextToSpeechRequest {
            text: "   ".to_string(),
            voice: None,
            format: AudioFormat::Wav,
            model_id: provider.default_model_id().clone(),
            mission_id: MissionId::new("mission.local-tts-test"),
            requested_at: 1,
        })
        .await
        .expect_err("empty text rejected locally");

    assert!(err.to_string().contains("non-empty"));
}
