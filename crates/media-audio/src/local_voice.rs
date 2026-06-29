//! Local voice providers for no-cloud speech-to-text and text-to-speech.
//!
//! The implementations in this module deliberately spawn a configured local
//! executable directly (no shell) so regulated deployments can bind Ardur voice
//! features to on-device engines such as whisper.cpp, Vosk, Piper, or the host
//! OS speech stack without sending audio/text to a network provider.

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use ardur_media_decode::AudioFormat;
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    AudioArtifactHandle, AudioCostEnvelope, AudioError, AudioHash, AudioInput, AudioModelId,
    AudioProviderId, AudioRequestRef, AudioScope, AudioVerb, AuthorizedAudioToken, ContentClass,
    DiarizationOutcome, DiarizationRequest, LanguageTag, MediaProvider, MissionId, ModelVersionPin,
    ReceiptHash, SegmentId, SpeakerLabel, StreamHandle, TranscribeFileRequest,
    TranscribeStreamRequest, Transcript, TranscriptHash, TranscriptSegment,
    TranscriptionCapabilitySurface, TranscriptionProvider, UnixTsMillis,
};

const LOCAL_STT_PROVIDER_ID: &str = "local-stt";
const LOCAL_STT_MODEL_ID: &str = "local-stt-command";
const LOCAL_TTS_PROVIDER_ID: &str = "local-tts";
const LOCAL_TTS_MODEL_ID: &str = "local-tts-command";
const MAX_LOCAL_DURATION_SECONDS: u32 = 4 * 60 * 60;
const MAX_INLINE_AUDIO_BYTES: usize = 64 * 1024 * 1024;
const MAX_TEXT_TO_SPEECH_CHARS: usize = 16 * 1024;

/// Configuration for a local command-backed STT provider.
#[derive(Clone, Debug)]
pub struct LocalSttConfig {
    /// Executable path to run. It is invoked directly without a shell.
    pub command: PathBuf,
    /// Argument template. `{input}`, `{model}`, and `{language}` placeholders
    /// are substituted per request. When empty, `{input}` is passed as the sole
    /// argument.
    pub args: Vec<String>,
    /// Provider id exposed to policy, receipts, and transcripts.
    pub provider_id: AudioProviderId,
    /// Default model id used by wrappers when a request omits a model.
    pub model_id: AudioModelId,
}

impl LocalSttConfig {
    /// Build a local STT config around a command path.
    #[must_use]
    pub fn new(command: impl Into<PathBuf>) -> Self {
        Self {
            command: command.into(),
            args: vec!["{input}".to_string()],
            provider_id: AudioProviderId::new(LOCAL_STT_PROVIDER_ID),
            model_id: AudioModelId::new(LOCAL_STT_MODEL_ID),
        }
    }

    /// Override the command argument template.
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Override the provider id.
    #[must_use]
    pub fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = AudioProviderId::new(provider_id);
        self
    }

    /// Override the default model id.
    #[must_use]
    pub fn with_model_id(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = AudioModelId::new(model_id);
        self
    }
}

/// Local command-backed speech-to-text provider.
#[derive(Clone, Debug)]
pub struct LocalSpeechToTextProvider {
    config: LocalSttConfig,
    receipt_tail: Arc<Mutex<Option<String>>>,
}

impl LocalSpeechToTextProvider {
    /// Construct a local STT provider, validating the command path is non-empty.
    pub fn new(config: LocalSttConfig) -> Result<Self, AudioError> {
        if config.command.as_os_str().is_empty() {
            return Err(AudioError::InvalidRequest(
                "local STT command path must be non-empty".to_string(),
            ));
        }
        Ok(Self {
            config,
            receipt_tail: Arc::new(Mutex::new(None)),
        })
    }

    /// Build from `ARDUR_LOCAL_STT_COMMAND`, returning `None` when unset.
    pub fn from_env() -> Result<Option<Self>, AudioError> {
        let Some(command) = std::env::var_os("ARDUR_LOCAL_STT_COMMAND") else {
            return Ok(None);
        };
        if command.is_empty() {
            return Ok(None);
        }
        let mut config = LocalSttConfig::new(PathBuf::from(command));
        if let Ok(model) = std::env::var("ARDUR_LOCAL_STT_MODEL") {
            if !model.trim().is_empty() {
                config = config.with_model_id(model);
            }
        }
        Self::new(config).map(Some)
    }

    fn chain_receipt_hash(
        &self,
        audio_hash: &AudioHash,
        transcript_hash: &TranscriptHash,
    ) -> ReceiptHash {
        let mut tail = self
            .receipt_tail
            .lock()
            .expect("local STT receipt tail lock poisoned");
        let parent = tail.clone().unwrap_or_else(|| "genesis".to_string());
        let digest = digest_hex(format!(
            "parent={parent};local_stt_audio={};transcript={}",
            audio_hash.as_str(),
            transcript_hash.as_str()
        ));
        *tail = Some(digest.clone());
        ReceiptHash::new(digest)
    }
}

impl MediaProvider for LocalSpeechToTextProvider {
    fn media_provider_id(&self) -> &AudioProviderId {
        &self.config.provider_id
    }
}

impl crate::private::Sealed for LocalSpeechToTextProvider {}

#[async_trait]
impl TranscriptionProvider for LocalSpeechToTextProvider {
    async fn transcribe_file(
        &self,
        token: &AuthorizedAudioToken,
        request: TranscribeFileRequest,
    ) -> Result<Transcript, AudioError> {
        validate_authorized_scope(token, &request)?;
        self.capability_surface().check_file_request(&request)?;

        let prepared = prepare_audio_input(&request.input)?;
        if prepared.bytes.is_empty() {
            return Err(AudioError::InvalidRequest(
                "audio bytes must be non-empty".to_string(),
            ));
        }
        let audio_hash = AudioHash::new(digest_hex(&prepared.bytes));
        let args = render_args(
            &self.config.args,
            &prepared.path,
            &request.model_id,
            request.language_hint.as_ref(),
        );
        let output = Command::new(&self.config.command)
            .args(args)
            .output()
            .map_err(|e| AudioError::Provider(format!("running local STT command: {e}")))?;
        if !output.status.success() {
            return Err(AudioError::Provider(format!(
                "local STT command exited with {}: {}",
                output.status,
                sanitize_command_output(&output.stderr)
            )));
        }
        let transcript_text = String::from_utf8(output.stdout)
            .map_err(|e| AudioError::Provider(format!("local STT output was not UTF-8: {e}")))?
            .trim()
            .to_string();
        if transcript_text.is_empty() {
            return Err(AudioError::Provider(
                "local STT command returned an empty transcript".to_string(),
            ));
        }
        let language = request
            .language_hint
            .clone()
            .unwrap_or(LanguageTag::new("und")?);
        let transcript_hash = TranscriptHash::new(digest_hex(transcript_text.as_bytes()));
        let receipt_hash = self.chain_receipt_hash(&audio_hash, &transcript_hash);

        Ok(Transcript {
            transcript_id: Default::default(),
            segments: vec![TranscriptSegment {
                segment_id: SegmentId::new(),
                start_seconds: 0.0,
                end_seconds: request.duration_seconds_upper_bound as f32,
                text: transcript_text,
                language: Some(language.clone()),
                speaker: Some(SpeakerLabel::Unknown),
                confidence: 1.0,
            }],
            language_detected: language,
            language_per_segment: Vec::new(),
            source_audio_handle: AudioArtifactHandle::new(format!("local:{}", audio_hash.as_str())),
            duration_seconds: request.duration_seconds_upper_bound,
            provider_id: self.config.provider_id.clone(),
            model_id: request.model_id,
            content_class: ContentClass::Safe,
            redaction_window_count: 0,
            created_at: request.requested_at,
            receipt_hash,
        })
    }

    async fn transcribe_stream(
        &self,
        _token: &AuthorizedAudioToken,
        request: TranscribeStreamRequest,
    ) -> Result<StreamHandle, AudioError> {
        request.validate()?;
        Err(AudioError::InvalidRequest(
            "local STT streaming sessions are not implemented; use transcribe_file".to_string(),
        ))
    }

    async fn apply_diarization(
        &self,
        _token: &AuthorizedAudioToken,
        request: DiarizationRequest,
    ) -> Result<DiarizationOutcome, AudioError> {
        Err(crate::AudioRefusalReason::DiarizationUnsupported {
            provider_id: request.provider_id,
        }
        .into())
    }

    fn cost_envelope(&self, verb: AudioVerb, request: AudioRequestRef<'_>) -> AudioCostEnvelope {
        match (verb, request) {
            (AudioVerb::TranscribeFile, AudioRequestRef::File(req)) => AudioCostEnvelope::File {
                duration_seconds: req.duration_seconds_upper_bound,
                per_second_micro_usd: 10,
                floor_micro_usd: 0,
                diarization_overhead_micro_usd: 0,
            },
            (AudioVerb::TranscribeStream, AudioRequestRef::Stream(req)) => {
                AudioCostEnvelope::Stream {
                    expected_duration_seconds_upper_bound: req
                        .expected_duration_seconds_upper_bound,
                    sample_cadence_seconds: req.sample_cadence_seconds,
                    per_second_micro_usd: 10,
                    connection_floor_micro_usd: 0,
                    per_sample_receipt_overhead_micro_usd: 1,
                }
            }
            _ => AudioCostEnvelope::File {
                duration_seconds: 0,
                per_second_micro_usd: 0,
                floor_micro_usd: 0,
                diarization_overhead_micro_usd: 0,
            },
        }
    }

    fn capability_surface(&self) -> TranscriptionCapabilitySurface {
        TranscriptionCapabilitySurface {
            supported_verbs: std::collections::BTreeSet::from([AudioVerb::TranscribeFile]),
            supported_input_formats: std::collections::BTreeSet::from([
                AudioFormat::Mp3,
                AudioFormat::Wav,
                AudioFormat::Opus,
                AudioFormat::Flac,
                AudioFormat::M4a,
                AudioFormat::WebmAudio,
            ]),
            max_duration_seconds_per_file: MAX_LOCAL_DURATION_SECONDS,
            supported_languages: std::collections::BTreeSet::new(),
            supports_diarization_native: false,
            supports_streaming: false,
            max_concurrent_streams: 0,
            model_version_pinned: Some(ModelVersionPin::new(self.config.model_id.as_str())),
        }
    }

    fn detect_language(&self, transcript: &Transcript) -> LanguageTag {
        transcript.language_detected.clone()
    }
}

/// Text-to-speech synthesis request.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TextToSpeechRequest {
    /// Text to synthesize.
    pub text: String,
    /// Optional voice identifier understood by the configured local engine.
    pub voice: Option<String>,
    /// Requested output format.
    pub format: AudioFormat,
    /// Model to invoke.
    pub model_id: AudioModelId,
    /// Mission id for audit linkage.
    pub mission_id: MissionId,
    /// Caller-observed request timestamp.
    pub requested_at: UnixTsMillis,
}

/// Text-to-speech synthesis result.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TextToSpeechOutput {
    /// Synthesized audio bytes.
    pub audio_bytes: Vec<u8>,
    /// Output format.
    pub format: AudioFormat,
    /// Estimated duration of the generated audio.
    pub duration_seconds: u32,
    /// Provider that produced the audio.
    pub provider_id: AudioProviderId,
    /// Model that produced the audio.
    pub model_id: AudioModelId,
    /// Creation timestamp.
    pub created_at: UnixTsMillis,
    /// Receipt hash proving the synthesis operation.
    pub receipt_hash: ReceiptHash,
}

/// Closed text-to-speech provider trait.
#[async_trait]
pub trait TextToSpeechProvider: MediaProvider + crate::private::Sealed {
    /// Synthesize speech audio for a text request.
    async fn synthesize(
        &self,
        request: TextToSpeechRequest,
    ) -> Result<TextToSpeechOutput, AudioError>;

    /// Return the provider's default model id.
    fn default_model_id(&self) -> &AudioModelId;
}

/// Configuration for a local command-backed TTS provider.
#[derive(Clone, Debug)]
pub struct LocalTtsConfig {
    /// Executable path to run. It is invoked directly without a shell.
    pub command: PathBuf,
    /// Argument template. `{text}`, `{voice}`, `{output}`, and `{model}` are
    /// substituted per request. If `{output}` is absent, stdout is treated as
    /// synthesized audio bytes.
    pub args: Vec<String>,
    /// Provider id exposed to policy, receipts, and tool output.
    pub provider_id: AudioProviderId,
    /// Default model id used by the provider.
    pub model_id: AudioModelId,
}

impl LocalTtsConfig {
    /// Build a local TTS config around a command path.
    #[must_use]
    pub fn new(command: impl Into<PathBuf>) -> Self {
        Self {
            command: command.into(),
            args: vec!["{text}".to_string()],
            provider_id: AudioProviderId::new(LOCAL_TTS_PROVIDER_ID),
            model_id: AudioModelId::new(LOCAL_TTS_MODEL_ID),
        }
    }

    /// Override the command argument template.
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Override the provider id.
    #[must_use]
    pub fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = AudioProviderId::new(provider_id);
        self
    }

    /// Override the default model id.
    #[must_use]
    pub fn with_model_id(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = AudioModelId::new(model_id);
        self
    }
}

/// Local command-backed text-to-speech provider.
#[derive(Clone, Debug)]
pub struct LocalTextToSpeechProvider {
    config: LocalTtsConfig,
    receipt_tail: Arc<Mutex<Option<String>>>,
}

impl LocalTextToSpeechProvider {
    /// Construct a local TTS provider, validating the command path is non-empty.
    pub fn new(config: LocalTtsConfig) -> Result<Self, AudioError> {
        if config.command.as_os_str().is_empty() {
            return Err(AudioError::InvalidRequest(
                "local TTS command path must be non-empty".to_string(),
            ));
        }
        Ok(Self {
            config,
            receipt_tail: Arc::new(Mutex::new(None)),
        })
    }

    /// Build from `ARDUR_LOCAL_TTS_COMMAND`, returning `None` when unset.
    pub fn from_env() -> Result<Option<Self>, AudioError> {
        let Some(command) = std::env::var_os("ARDUR_LOCAL_TTS_COMMAND") else {
            return Ok(None);
        };
        if command.is_empty() {
            return Ok(None);
        }
        let mut config = LocalTtsConfig::new(PathBuf::from(command));
        if let Ok(model) = std::env::var("ARDUR_LOCAL_TTS_MODEL") {
            if !model.trim().is_empty() {
                config = config.with_model_id(model);
            }
        }
        Self::new(config).map(Some)
    }

    fn chain_receipt_hash(&self, text: &str, audio_bytes: &[u8]) -> ReceiptHash {
        let mut tail = self
            .receipt_tail
            .lock()
            .expect("local TTS receipt tail lock poisoned");
        let parent = tail.clone().unwrap_or_else(|| "genesis".to_string());
        let digest = digest_hex(format!(
            "parent={parent};local_tts_text={};audio={}",
            digest_hex(text.as_bytes()),
            digest_hex(audio_bytes)
        ));
        *tail = Some(digest.clone());
        ReceiptHash::new(digest)
    }
}

impl MediaProvider for LocalTextToSpeechProvider {
    fn media_provider_id(&self) -> &AudioProviderId {
        &self.config.provider_id
    }
}

impl crate::private::Sealed for LocalTextToSpeechProvider {}

#[async_trait]
impl TextToSpeechProvider for LocalTextToSpeechProvider {
    async fn synthesize(
        &self,
        request: TextToSpeechRequest,
    ) -> Result<TextToSpeechOutput, AudioError> {
        let text = request.text.trim();
        if text.is_empty() {
            return Err(AudioError::InvalidRequest(
                "text-to-speech text must be non-empty".to_string(),
            ));
        }
        if request.text.chars().count() > MAX_TEXT_TO_SPEECH_CHARS {
            return Err(AudioError::InvalidRequest(format!(
                "text-to-speech text exceeds {MAX_TEXT_TO_SPEECH_CHARS} characters"
            )));
        }
        let output_path = if args_contain_output(&self.config.args) {
            Some(temp_audio_path(
                "ardur-local-tts",
                extension_for_format(request.format),
            ))
        } else {
            None
        };
        let args = render_tts_args(&self.config.args, &request, output_path.as_deref());
        let output = Command::new(&self.config.command)
            .args(args)
            .output()
            .map_err(|e| AudioError::Provider(format!("running local TTS command: {e}")))?;
        if !output.status.success() {
            return Err(AudioError::Provider(format!(
                "local TTS command exited with {}: {}",
                output.status,
                sanitize_command_output(&output.stderr)
            )));
        }
        let audio_bytes = match output_path {
            Some(path) => {
                let bytes = std::fs::read(&path)
                    .map_err(|e| AudioError::Provider(format!("reading local TTS output: {e}")))?;
                let _ = std::fs::remove_file(path);
                bytes
            }
            None => output.stdout,
        };
        if audio_bytes.is_empty() {
            return Err(AudioError::Provider(
                "local TTS command returned no audio bytes".to_string(),
            ));
        }
        let receipt_hash = self.chain_receipt_hash(text, &audio_bytes);
        Ok(TextToSpeechOutput {
            audio_bytes,
            format: request.format,
            duration_seconds: estimate_speech_duration_seconds(text),
            provider_id: self.config.provider_id.clone(),
            model_id: request.model_id,
            created_at: request.requested_at,
            receipt_hash,
        })
    }

    fn default_model_id(&self) -> &AudioModelId {
        &self.config.model_id
    }
}

/// Tool wrapper that exposes local TTS as `voice.speak`.
pub struct VoiceSpeakTool {
    provider: Arc<dyn TextToSpeechProvider>,
    schema: ardur_tool_registry::ToolSchema,
    capabilities: Vec<ardur_tool_registry::Capability>,
}

impl VoiceSpeakTool {
    /// Stable tool id registered in [`ardur_tool_registry::ToolRegistry`].
    pub const ID: &'static str = "voice.speak";

    /// Wrap a concrete local TTS provider as the `voice.speak` tool.
    #[must_use]
    pub fn new(provider: impl TextToSpeechProvider + 'static) -> Self {
        Self::from_arc(Arc::new(provider))
    }

    /// Wrap an already-shared TTS provider as the `voice.speak` tool.
    #[must_use]
    pub fn from_arc(provider: Arc<dyn TextToSpeechProvider>) -> Self {
        Self {
            provider,
            schema: voice_speak_schema(),
            capabilities: vec![
                ardur_tool_registry::Capability::VoiceOutput,
                ardur_tool_registry::Capability::ProcessSpawn,
                ardur_tool_registry::Capability::FsRead,
                ardur_tool_registry::Capability::FsWrite,
                ardur_tool_registry::Capability::Custom(Self::ID.to_string()),
            ],
        }
    }
}

#[async_trait]
impl ardur_tool_registry::Tool for VoiceSpeakTool {
    fn id(&self) -> ardur_tool_registry::ToolId {
        ardur_tool_registry::ToolId::new(Self::ID)
    }

    fn schema(&self) -> &ardur_tool_registry::ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        ctx: &ardur_tool_registry::ToolContext,
        args: serde_json::Value,
    ) -> Result<ardur_tool_registry::ToolOutput, ardur_tool_registry::ToolError> {
        if ctx.cap_token.0.trim().is_empty() {
            return Err(ardur_tool_registry::ToolError::Denied {
                reason: "missing cap-token for voice.speak".to_string(),
            });
        }
        let parsed = VoiceSpeakArgs::parse(args, self.provider.default_model_id().clone())?;
        let output = self
            .provider
            .synthesize(parsed.request)
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;
        Ok(ardur_tool_registry::ToolOutput {
            content: serde_json::json!({
                "audio_base64": BASE64_STANDARD.encode(&output.audio_bytes),
                "format": format_name(output.format),
                "duration_seconds": output.duration_seconds,
                "provider_id": output.provider_id.as_str(),
                "model_id": output.model_id.as_str(),
                "receipt_hash": output.receipt_hash.as_str(),
            }),
            cost: ardur_runtime::CostTuple {
                cents: 0,
                wall_ms: 0,
                ..Default::default()
            },
            receipt_data: serde_json::json!({
                "operation": Self::ID,
                "provider_id": output.provider_id.as_str(),
                "model_id": output.model_id.as_str(),
                "receipt_hash": output.receipt_hash.as_str(),
                "audio_bytes": output.audio_bytes.len(),
            }),
        })
    }

    fn required_capabilities(&self) -> &[ardur_tool_registry::Capability] {
        &self.capabilities
    }
}

#[derive(Debug)]
struct VoiceSpeakArgs {
    request: TextToSpeechRequest,
}

impl VoiceSpeakArgs {
    fn parse(
        value: serde_json::Value,
        default_model: AudioModelId,
    ) -> Result<Self, ardur_tool_registry::ToolError> {
        let object = value.as_object().ok_or_else(|| {
            ardur_tool_registry::ToolError::InvalidArgs(
                "arguments must be a JSON object".to_string(),
            )
        })?;
        let text = object
            .get("text")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                ardur_tool_registry::ToolError::InvalidArgs(
                    "text is required and must be non-empty".to_string(),
                )
            })?
            .to_string();
        let voice = object
            .get("voice")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToString::to_string);
        let format = object
            .get("format")
            .and_then(serde_json::Value::as_str)
            .map(parse_audio_format)
            .transpose()?
            .unwrap_or(AudioFormat::Wav);
        let model_id = object
            .get("model_id")
            .and_then(serde_json::Value::as_str)
            .map(AudioModelId::new)
            .unwrap_or(default_model);
        let mission_id = object
            .get("mission_id")
            .and_then(serde_json::Value::as_str)
            .map(MissionId::new)
            .unwrap_or_else(|| MissionId::new("voice.speak"));
        Ok(Self {
            request: TextToSpeechRequest {
                text,
                voice,
                format,
                model_id,
                mission_id,
                requested_at: unix_millis_now(),
            },
        })
    }
}

struct PreparedAudioInput {
    path: PathBuf,
    bytes: Vec<u8>,
    cleanup: bool,
}

impl Drop for PreparedAudioInput {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn prepare_audio_input(input: &AudioInput) -> Result<PreparedAudioInput, AudioError> {
    match input {
        AudioInput::InlineBytes { bytes, format } => {
            if bytes.len() > MAX_INLINE_AUDIO_BYTES {
                return Err(AudioError::InvalidRequest(format!(
                    "inline audio exceeds {MAX_INLINE_AUDIO_BYTES} bytes"
                )));
            }
            let path = temp_audio_path("ardur-local-stt", extension_for_format(*format));
            std::fs::write(&path, bytes)
                .map_err(|e| AudioError::Provider(format!("writing temp audio: {e}")))?;
            Ok(PreparedAudioInput {
                path,
                bytes: bytes.clone(),
                cleanup: true,
            })
        }
        AudioInput::WorkspacePath { path, .. } => {
            ensure_workspace_relative_path(path)?;
            let bytes = std::fs::read(path)
                .map_err(|e| AudioError::Provider(format!("reading workspace audio: {e}")))?;
            Ok(PreparedAudioInput {
                path: path.clone(),
                bytes,
                cleanup: false,
            })
        }
        AudioInput::Artifact { .. } => Err(AudioError::InvalidRequest(
            "artifact audio input is not supported by LocalSpeechToTextProvider".to_string(),
        )),
    }
}

fn validate_authorized_scope(
    token: &AuthorizedAudioToken,
    request: &TranscribeFileRequest,
) -> Result<(), AudioError> {
    if token.cap_token.0.trim().is_empty() {
        return Err(AudioError::InvalidRequest(
            "cap-token must be non-empty".to_string(),
        ));
    }
    if token.scope.verb != AudioVerb::TranscribeFile
        || token.scope.provider_id != request.provider_id
    {
        return Err(crate::AudioRefusalReason::CapTokenInsufficient {
            required_scope: AudioScope {
                verb: AudioVerb::TranscribeFile,
                provider_id: request.provider_id.clone(),
                duration_seconds_ceiling: request.duration_seconds_upper_bound,
                content_class_ceiling: ContentClass::Safe,
            },
        }
        .into());
    }
    if request.duration_seconds_upper_bound > token.scope.duration_seconds_ceiling {
        return Err(crate::AudioRefusalReason::DurationCeilingExceeded {
            requested: request.duration_seconds_upper_bound,
            ceiling: token.scope.duration_seconds_ceiling,
        }
        .into());
    }
    Ok(())
}

fn ensure_workspace_relative_path(path: &Path) -> Result<(), AudioError> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(crate::AudioRefusalReason::WorkspaceScopeRefused {
            path: path.to_path_buf(),
        }
        .into());
    }
    Ok(())
}

fn render_args(
    template: &[String],
    input_path: &Path,
    model_id: &AudioModelId,
    language: Option<&LanguageTag>,
) -> Vec<String> {
    let args = if template.is_empty() {
        vec!["{input}".to_string()]
    } else {
        template.to_vec()
    };
    args.into_iter()
        .map(|arg| {
            arg.replace("{input}", &input_path.to_string_lossy())
                .replace("{model}", model_id.as_str())
                .replace(
                    "{language}",
                    language.map(LanguageTag::as_str).unwrap_or(""),
                )
        })
        .collect()
}

fn render_tts_args(
    template: &[String],
    request: &TextToSpeechRequest,
    output_path: Option<&Path>,
) -> Vec<String> {
    let args = if template.is_empty() {
        vec!["{text}".to_string()]
    } else {
        template.to_vec()
    };
    args.into_iter()
        .map(|arg| {
            arg.replace("{text}", &request.text)
                .replace("{voice}", request.voice.as_deref().unwrap_or(""))
                .replace("{model}", request.model_id.as_str())
                .replace(
                    "{output}",
                    &output_path
                        .map(|path| path.to_string_lossy().to_string())
                        .unwrap_or_default(),
                )
        })
        .collect()
}

fn args_contain_output(args: &[String]) -> bool {
    args.iter().any(|arg| arg.contains("{output}"))
}

fn temp_audio_path(prefix: &str, extension: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{prefix}-{}.{}", Uuid::new_v4(), extension))
}

fn parse_audio_format(raw: &str) -> Result<AudioFormat, ardur_tool_registry::ToolError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "mp3" => Ok(AudioFormat::Mp3),
        "wav" | "wave" => Ok(AudioFormat::Wav),
        "ogg" | "opus" => Ok(AudioFormat::Opus),
        "flac" => Ok(AudioFormat::Flac),
        "aac" | "m4a" | "mp4" => Ok(AudioFormat::M4a),
        "webm" => Ok(AudioFormat::WebmAudio),
        other => Err(ardur_tool_registry::ToolError::InvalidArgs(format!(
            "unsupported audio format `{other}`"
        ))),
    }
}

fn format_name(format: AudioFormat) -> &'static str {
    match format {
        AudioFormat::Mp3 => "mp3",
        AudioFormat::Wav => "wav",
        AudioFormat::Opus => "opus",
        AudioFormat::Flac => "flac",
        AudioFormat::M4a => "m4a",
        AudioFormat::WebmAudio => "webm",
    }
}

fn extension_for_format(format: AudioFormat) -> &'static str {
    format_name(format)
}

fn estimate_speech_duration_seconds(text: &str) -> u32 {
    let words = text.split_whitespace().count().max(1) as u32;
    words.div_ceil(2).max(1)
}

fn digest_hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}

fn sanitize_command_output(body: &[u8]) -> String {
    let clipped = &body[..body.len().min(512)];
    let mut message = String::from_utf8_lossy(clipped).to_string();
    if body.len() > clipped.len() {
        message.push_str("...<truncated>");
    }
    message.replace(|ch: char| ch.is_control() && ch != '\n' && ch != '\t', " ")
}

fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn voice_speak_schema() -> ardur_tool_registry::ToolSchema {
    ardur_tool_registry::ToolSchema {
        description: "Synthesize speech locally from text with the configured no-cloud TTS engine."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["text"],
            "properties": {
                "text": { "type": "string", "description": "Text to synthesize." },
                "voice": { "type": "string", "description": "Optional local voice id." },
                "format": { "type": "string", "enum": ["mp3", "wav", "ogg", "opus", "flac", "m4a", "webm"], "default": "wav" },
                "model_id": { "type": "string" },
                "mission_id": { "type": "string" }
            }
        }),
        output_schema: serde_json::json!({
            "type": "object",
            "required": ["audio_base64", "format", "receipt_hash"],
            "properties": {
                "audio_base64": { "type": "string" },
                "format": { "type": "string" },
                "duration_seconds": { "type": "integer" },
                "provider_id": { "type": "string" },
                "model_id": { "type": "string" },
                "receipt_hash": { "type": "string" }
            }
        }),
        examples: vec![ardur_tool_registry::ToolExample {
            description: "Read a short approval summary aloud.".to_string(),
            args: serde_json::json!({ "text": "Approve the draft email?", "voice": "default" }),
            output: serde_json::json!({
                "audio_base64": "UklGRgAAAApXQVZF",
                "format": "wav",
                "duration_seconds": 3,
                "provider_id": LOCAL_TTS_PROVIDER_ID,
                "model_id": LOCAL_TTS_MODEL_ID,
                "receipt_hash": "..."
            }),
        }],
    }
}
