//! OpenAI Whisper API transcription provider and receipt-chained voice tool.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ardur_media_decode::AudioFormat;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    AudioArtifactHandle, AudioCostEnvelope, AudioError, AudioHash, AudioInput, AudioModelId,
    AudioProviderId, AudioRequestRef, AudioScope, AudioVerb, AuthorizedAudioToken, ContentClass,
    DiarizationMode, DiarizationOutcome, DiarizationRequest, LanguageTag, MediaProvider, MissionId,
    ModelVersionPin, ReceiptHash, SegmentId, SpeakerLabel, StreamHandle, TranscribeFileRequest,
    TranscribeStreamRequest, Transcript, TranscriptFormat, TranscriptHash, TranscriptSegment,
    TranscriptionCapabilitySurface, TranscriptionProvider, UnixTsMillis,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1/";
const DEFAULT_MODEL: &str = "whisper-1";
const DEFAULT_PROVIDER_ID: &str = "openai-whisper";
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_DURATION_SECONDS: u32 = 2 * 60 * 60;
const MAX_INLINE_AUDIO_BYTES: usize = 25 * 1024 * 1024;
const MAX_UPSTREAM_ERROR_BYTES: usize = 512;

/// Configuration for [`WhisperApiTranscriptionProvider`].
#[derive(Clone)]
pub struct WhisperApiConfig {
    api_key: String,
    base_url: Url,
    model: AudioModelId,
    provider_id: AudioProviderId,
    timeout: Duration,
}

impl std::fmt::Debug for WhisperApiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhisperApiConfig")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url.as_str())
            .field("model", &self.model)
            .field("provider_id", &self.provider_id)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl WhisperApiConfig {
    /// Build a config using the OpenAI default `/v1` base URL and `whisper-1`.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: Url::parse(DEFAULT_BASE_URL).expect("default Whisper URL is valid"),
            model: AudioModelId::new(DEFAULT_MODEL),
            provider_id: AudioProviderId::new(DEFAULT_PROVIDER_ID),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }

    /// Override the API base URL. HTTPS is required except for loopback HTTP,
    /// which is allowed for deterministic integration tests.
    pub fn with_base_url(mut self, base_url: impl AsRef<str>) -> Result<Self, AudioError> {
        self.base_url = validate_base_url(base_url.as_ref())?;
        Ok(self)
    }

    /// Override the default Whisper model id.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = AudioModelId::new(model);
        self
    }

    /// Override the provider id exposed to policy and receipts.
    #[must_use]
    pub fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = AudioProviderId::new(provider_id);
        self
    }

    /// Override the HTTP request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// OpenAI Whisper API-backed transcription provider.
#[derive(Clone)]
pub struct WhisperApiTranscriptionProvider {
    config: WhisperApiConfig,
    client: reqwest::Client,
    receipt_tail: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for WhisperApiTranscriptionProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhisperApiTranscriptionProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl WhisperApiTranscriptionProvider {
    /// Construct a provider, validating that the API key and HTTP client config
    /// are usable before the provider is registered.
    pub fn new(config: WhisperApiConfig) -> Result<Self, AudioError> {
        if config.api_key.trim().is_empty() {
            return Err(AudioError::InvalidRequest(
                "Whisper API key must be non-empty".to_string(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| AudioError::Provider(format!("building Whisper HTTP client: {e}")))?;
        Ok(Self {
            config,
            client,
            receipt_tail: Arc::new(Mutex::new(None)),
        })
    }

    /// Build from `OPENAI_WHISPER_API_KEY` or `OPENAI_API_KEY`, returning `None`
    /// when no key is configured so server boot can degrade gracefully.
    pub fn from_env() -> Result<Option<Self>, AudioError> {
        let key = std::env::var("OPENAI_WHISPER_API_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("OPENAI_API_KEY")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            });
        let Some(key) = key else {
            return Ok(None);
        };
        let mut config = WhisperApiConfig::new(key);
        if let Ok(base_url) = std::env::var("OPENAI_WHISPER_BASE_URL") {
            if !base_url.trim().is_empty() {
                config = config.with_base_url(base_url)?;
            }
        }
        if let Ok(model) = std::env::var("OPENAI_WHISPER_MODEL") {
            if !model.trim().is_empty() {
                config = config.with_model(model);
            }
        }
        Self::new(config).map(Some)
    }

    fn endpoint(&self) -> Result<Url, AudioError> {
        self.config
            .base_url
            .join("audio/transcriptions")
            .map_err(|e| AudioError::InvalidRequest(format!("invalid Whisper endpoint: {e}")))
    }

    fn chain_receipt_hash(
        &self,
        audio_hash: &AudioHash,
        transcript_hash: &TranscriptHash,
    ) -> ReceiptHash {
        let mut tail = self
            .receipt_tail
            .lock()
            .expect("Whisper provider receipt tail lock poisoned");
        let parent = tail.clone().unwrap_or_else(|| "genesis".to_string());
        let digest = digest_hex(format!(
            "parent={parent};audio={};transcript={}",
            audio_hash.as_str(),
            transcript_hash.as_str()
        ));
        *tail = Some(digest.clone());
        ReceiptHash::new(digest)
    }
}

impl MediaProvider for WhisperApiTranscriptionProvider {
    fn media_provider_id(&self) -> &AudioProviderId {
        &self.config.provider_id
    }
}

impl crate::private::Sealed for WhisperApiTranscriptionProvider {}

#[async_trait::async_trait]
impl TranscriptionProvider for WhisperApiTranscriptionProvider {
    async fn transcribe_file(
        &self,
        token: &AuthorizedAudioToken,
        request: TranscribeFileRequest,
    ) -> Result<Transcript, AudioError> {
        validate_authorized_scope(token, &request)?;
        self.capability_surface().check_file_request(&request)?;

        let bytes = load_audio_bytes(&request.input)?;
        if bytes.is_empty() {
            return Err(AudioError::InvalidRequest(
                "audio bytes must be non-empty".to_string(),
            ));
        }
        let audio_hash = AudioHash::new(digest_hex(&bytes));
        let (mime, ext) = mime_and_extension(request.input.format());
        let part = Part::bytes(bytes)
            .file_name(format!("audio.{ext}"))
            .mime_str(mime)
            .map_err(|e| AudioError::InvalidRequest(format!("invalid audio mime type: {e}")))?;
        let mut form = Form::new()
            .part("file", part)
            .text("model", request.model_id.as_str().to_string())
            .text("response_format", "verbose_json".to_string());
        if let Some(language) = &request.language_hint {
            form = form.text("language", language.as_str().to_string());
        }

        let response = self
            .client
            .post(self.endpoint()?)
            .bearer_auth(self.config.api_key.as_str())
            .multipart(form)
            .send()
            .await
            .map_err(|e| AudioError::Provider(format!("calling Whisper API: {e}")))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| AudioError::Provider(format!("reading Whisper response: {e}")))?;
        if !status.is_success() {
            return Err(AudioError::Provider(format!(
                "Whisper API returned {status}: {}",
                sanitize_upstream_error(&body)
            )));
        }
        let whisper: WhisperTranscriptionResponse = serde_json::from_slice(&body)
            .map_err(|e| AudioError::Provider(format!("parsing Whisper response: {e}")))?;
        if whisper.text.trim().is_empty() {
            return Err(AudioError::Provider(
                "Whisper API returned an empty transcript".to_string(),
            ));
        }

        let transcript_hash = TranscriptHash::new(digest_hex(whisper.text.as_bytes()));
        let receipt_hash = self.chain_receipt_hash(&audio_hash, &transcript_hash);
        let language = LanguageTag::new(
            whisper
                .language
                .clone()
                .filter(|lang| !lang.trim().is_empty())
                .unwrap_or_else(|| {
                    request
                        .language_hint
                        .as_ref()
                        .map(|lang| lang.as_str().to_string())
                        .unwrap_or_else(|| "und".to_string())
                }),
        )?;
        let segments =
            whisper_to_segments(&whisper, request.duration_seconds_upper_bound, &language);

        Ok(Transcript {
            transcript_id: Default::default(),
            segments,
            language_detected: language.clone(),
            language_per_segment: Vec::new(),
            source_audio_handle: AudioArtifactHandle::new(format!(
                "inline:{}",
                audio_hash.as_str()
            )),
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
            "Whisper API streaming transcription is not implemented; use transcribe_file"
                .to_string(),
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
                per_second_micro_usd: 100,
                floor_micro_usd: 1_000,
                diarization_overhead_micro_usd: 0,
            },
            (AudioVerb::TranscribeStream, AudioRequestRef::Stream(req)) => {
                AudioCostEnvelope::Stream {
                    expected_duration_seconds_upper_bound: req
                        .expected_duration_seconds_upper_bound,
                    sample_cadence_seconds: req.sample_cadence_seconds,
                    per_second_micro_usd: 100,
                    connection_floor_micro_usd: 1_000,
                    per_sample_receipt_overhead_micro_usd: 10,
                }
            }
            (AudioVerb::ApplyDiarization, AudioRequestRef::Diarization(_)) => {
                AudioCostEnvelope::Diarization {
                    segmented_duration_seconds: 0,
                    per_second_micro_usd: 0,
                    floor_micro_usd: 0,
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
        use std::collections::BTreeSet;
        TranscriptionCapabilitySurface {
            supported_verbs: BTreeSet::from([AudioVerb::TranscribeFile]),
            supported_input_formats: BTreeSet::from([
                AudioFormat::Mp3,
                AudioFormat::Wav,
                AudioFormat::Opus,
                AudioFormat::Flac,
                AudioFormat::M4a,
                AudioFormat::WebmAudio,
            ]),
            max_duration_seconds_per_file: MAX_DURATION_SECONDS,
            supported_languages: BTreeSet::new(),
            supports_diarization_native: false,
            supports_streaming: false,
            max_concurrent_streams: 0,
            model_version_pinned: Some(ModelVersionPin::new(DEFAULT_MODEL)),
        }
    }

    fn detect_language(&self, transcript: &Transcript) -> LanguageTag {
        transcript.language_detected.clone()
    }
}

/// Tool wrapper that exposes a transcription provider as `voice.transcribe`.
pub struct VoiceTranscribeTool {
    provider: Arc<dyn TranscriptionProvider>,
    schema: ardur_tool_registry::ToolSchema,
    capabilities: Vec<ardur_tool_registry::Capability>,
    default_model: AudioModelId,
}

impl VoiceTranscribeTool {
    /// Stable tool id registered in [`ardur_tool_registry::ToolRegistry`].
    pub const ID: &'static str = "voice.transcribe";

    /// Wrap a concrete provider as the `voice.transcribe` tool.
    #[must_use]
    pub fn new(provider: impl TranscriptionProvider + 'static) -> Self {
        Self::from_arc(Arc::new(provider))
    }

    /// Wrap an already-shared provider as the `voice.transcribe` tool.
    #[must_use]
    pub fn from_arc(provider: Arc<dyn TranscriptionProvider>) -> Self {
        Self {
            provider,
            schema: voice_transcribe_schema(),
            capabilities: vec![
                ardur_tool_registry::Capability::NetworkOut,
                ardur_tool_registry::Capability::Custom(Self::ID.to_string()),
            ],
            default_model: AudioModelId::new(DEFAULT_MODEL),
        }
    }
}

#[async_trait::async_trait]
impl ardur_tool_registry::Tool for VoiceTranscribeTool {
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
                reason: "missing cap-token for voice.transcribe".to_string(),
            });
        }
        let parsed = VoiceTranscribeArgs::parse(args)?;
        let request = parsed.to_request(
            self.provider.media_provider_id().clone(),
            self.default_model.clone(),
        )?;
        let token = AuthorizedAudioToken {
            cap_token: ctx.cap_token.clone(),
            scope: AudioScope {
                verb: AudioVerb::TranscribeFile,
                provider_id: self.provider.media_provider_id().clone(),
                duration_seconds_ceiling: MAX_DURATION_SECONDS,
                content_class_ceiling: ContentClass::Safe,
            },
        };
        let envelope = self
            .provider
            .cost_envelope(AudioVerb::TranscribeFile, AudioRequestRef::File(&request));
        let transcript = self
            .provider
            .transcribe_file(&token, request)
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;
        let text = transcript
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let cents = envelope.total_micro_usd().div_ceil(10_000);
        Ok(ardur_tool_registry::ToolOutput {
            content: serde_json::json!({
                "text": text,
                "language": transcript.language_detected.as_str(),
                "duration_seconds": transcript.duration_seconds,
                "provider_id": transcript.provider_id.as_str(),
                "model_id": transcript.model_id.as_str(),
                "receipt_hash": transcript.receipt_hash.as_str(),
            }),
            cost: ardur_runtime::CostTuple {
                cents,
                wall_ms: 0,
                ..Default::default()
            },
            receipt_data: serde_json::json!({
                "operation": Self::ID,
                "provider_id": transcript.provider_id.as_str(),
                "model_id": transcript.model_id.as_str(),
                "transcript_id": transcript.transcript_id,
                "receipt_hash": transcript.receipt_hash.as_str(),
                "cost_envelope_micro_usd": envelope.total_micro_usd(),
            }),
        })
    }

    fn required_capabilities(&self) -> &[ardur_tool_registry::Capability] {
        &self.capabilities
    }
}

#[derive(Debug)]
struct VoiceTranscribeArgs {
    audio_base64: String,
    format: AudioFormat,
    duration_seconds_upper_bound: u32,
    language_hint: Option<LanguageTag>,
    model_id: Option<AudioModelId>,
    mission_id: MissionId,
}

impl VoiceTranscribeArgs {
    fn parse(value: serde_json::Value) -> Result<Self, ardur_tool_registry::ToolError> {
        let object = value.as_object().ok_or_else(|| {
            ardur_tool_registry::ToolError::InvalidArgs(
                "arguments must be a JSON object".to_string(),
            )
        })?;
        let audio_base64 = object
            .get("audio_base64")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                ardur_tool_registry::ToolError::InvalidArgs(
                    "audio_base64 is required and must be non-empty".to_string(),
                )
            })?
            .to_string();
        let format = object
            .get("format")
            .and_then(serde_json::Value::as_str)
            .map(parse_audio_format)
            .transpose()?
            .unwrap_or(AudioFormat::Wav);
        let duration_seconds_upper_bound = object
            .get("duration_seconds_upper_bound")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                ardur_tool_registry::ToolError::InvalidArgs(
                    "duration_seconds_upper_bound must be a positive integer".to_string(),
                )
            })?;
        if duration_seconds_upper_bound > MAX_DURATION_SECONDS {
            return Err(ardur_tool_registry::ToolError::InvalidArgs(format!(
                "duration_seconds_upper_bound {duration_seconds_upper_bound} exceeds maximum {MAX_DURATION_SECONDS}"
            )));
        }
        let language_hint = object
            .get("language_hint")
            .and_then(serde_json::Value::as_str)
            .map(LanguageTag::new)
            .transpose()
            .map_err(|e| ardur_tool_registry::ToolError::InvalidArgs(e.to_string()))?;
        let model_id = object
            .get("model_id")
            .and_then(serde_json::Value::as_str)
            .map(AudioModelId::new);
        let mission_id = object
            .get("mission_id")
            .and_then(serde_json::Value::as_str)
            .map(MissionId::new)
            .unwrap_or_else(|| MissionId::new("voice.transcribe"));
        Ok(Self {
            audio_base64,
            format,
            duration_seconds_upper_bound,
            language_hint,
            model_id,
            mission_id,
        })
    }

    fn to_request(
        &self,
        provider_id: AudioProviderId,
        default_model: AudioModelId,
    ) -> Result<TranscribeFileRequest, ardur_tool_registry::ToolError> {
        let bytes = BASE64_STANDARD
            .decode(self.audio_base64.as_bytes())
            .map_err(|e| {
                ardur_tool_registry::ToolError::InvalidArgs(format!("invalid audio_base64: {e}"))
            })?;
        if bytes.len() > MAX_INLINE_AUDIO_BYTES {
            return Err(ardur_tool_registry::ToolError::InvalidArgs(format!(
                "audio_base64 decodes to {} bytes, exceeding maximum {MAX_INLINE_AUDIO_BYTES}",
                bytes.len()
            )));
        }
        Ok(TranscribeFileRequest {
            provider_id,
            model_id: self.model_id.clone().unwrap_or(default_model),
            input: AudioInput::InlineBytes {
                bytes,
                format: self.format,
            },
            duration_seconds_upper_bound: self.duration_seconds_upper_bound,
            language_hint: self.language_hint.clone(),
            target_language: None,
            diarization: DiarizationMode::None,
            max_speakers: None,
            export_format: TranscriptFormat::Json,
            mission_id: self.mission_id.clone(),
            requested_at: unix_millis_now(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct WhisperTranscriptionResponse {
    text: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    segments: Vec<WhisperSegment>,
}

#[derive(Debug, Deserialize)]
struct WhisperSegment {
    #[serde(default)]
    start: f32,
    #[serde(default)]
    end: f32,
    #[serde(default)]
    text: String,
}

fn whisper_to_segments(
    response: &WhisperTranscriptionResponse,
    duration_seconds: u32,
    language: &LanguageTag,
) -> Vec<TranscriptSegment> {
    if response.segments.is_empty() {
        return vec![TranscriptSegment {
            segment_id: SegmentId::new(),
            start_seconds: 0.0,
            end_seconds: duration_seconds as f32,
            text: response.text.clone(),
            language: Some(language.clone()),
            speaker: Some(SpeakerLabel::Unknown),
            confidence: 1.0,
        }];
    }
    response
        .segments
        .iter()
        .filter(|segment| !segment.text.trim().is_empty())
        .map(|segment| TranscriptSegment {
            segment_id: SegmentId::new(),
            start_seconds: segment.start,
            end_seconds: segment.end,
            text: segment.text.trim().to_string(),
            language: Some(language.clone()),
            speaker: Some(SpeakerLabel::Unknown),
            confidence: 1.0,
        })
        .collect()
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

fn load_audio_bytes(input: &AudioInput) -> Result<Vec<u8>, AudioError> {
    match input {
        AudioInput::InlineBytes { bytes, .. } => Ok(bytes.clone()),
        AudioInput::WorkspacePath { path, .. } => read_workspace_audio(path),
        AudioInput::Artifact { .. } => Err(AudioError::InvalidRequest(
            "artifact audio input is not supported by WhisperApiTranscriptionProvider".to_string(),
        )),
    }
}

fn read_workspace_audio(path: &Path) -> Result<Vec<u8>, AudioError> {
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
    std::fs::read(PathBuf::from(path))
        .map_err(|e| AudioError::Provider(format!("reading workspace audio: {e}")))
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

fn mime_and_extension(format: AudioFormat) -> (&'static str, &'static str) {
    match format {
        AudioFormat::Mp3 => ("audio/mpeg", "mp3"),
        AudioFormat::Wav => ("audio/wav", "wav"),
        AudioFormat::Opus => ("audio/ogg", "opus"),
        AudioFormat::Flac => ("audio/flac", "flac"),
        AudioFormat::M4a => ("audio/mp4", "m4a"),
        AudioFormat::WebmAudio => ("audio/webm", "webm"),
    }
}

fn validate_base_url(raw: &str) -> Result<Url, AudioError> {
    let mut parsed = Url::parse(raw)
        .map_err(|e| AudioError::InvalidRequest(format!("invalid Whisper base URL: {e}")))?;
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(AudioError::InvalidRequest(
            "Whisper base URL must not contain query or fragment components".to_string(),
        ));
    }
    match parsed.scheme() {
        "https" => normalize_directory_base_url(&mut parsed),
        "http" if is_loopback_url(&parsed) => normalize_directory_base_url(&mut parsed),
        scheme => Err(AudioError::InvalidRequest(format!(
            "Whisper base URL must be HTTPS or loopback HTTP, got `{scheme}`"
        ))),
    }
}

fn normalize_directory_base_url(url: &mut Url) -> Result<Url, AudioError> {
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url.clone())
}

fn is_loopback_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host == "localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn digest_hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}

fn sanitize_upstream_error(body: &[u8]) -> String {
    let clipped = &body[..body.len().min(MAX_UPSTREAM_ERROR_BYTES)];
    let mut message = String::from_utf8_lossy(clipped).to_string();
    if body.len() > clipped.len() {
        message.push_str("...<truncated>");
    }
    message.replace(|ch: char| ch.is_control() && ch != '\n' && ch != '\t', " ")
}

fn unix_millis_now() -> UnixTsMillis {
    UnixTsMillis(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0),
    )
}

fn voice_transcribe_schema() -> ardur_tool_registry::ToolSchema {
    ardur_tool_registry::ToolSchema {
        description:
            "Transcribe a small base64-encoded audio clip with the configured Whisper API provider."
                .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["audio_base64", "duration_seconds_upper_bound"],
            "properties": {
                "audio_base64": { "type": "string", "description": "Base64 encoded audio bytes." },
                "format": { "type": "string", "enum": ["mp3", "wav", "ogg", "opus", "flac", "m4a", "webm"], "default": "wav" },
                "duration_seconds_upper_bound": { "type": "integer", "minimum": 1 },
                "language_hint": { "type": "string" },
                "model_id": { "type": "string" },
                "mission_id": { "type": "string" }
            }
        }),
        output_schema: serde_json::json!({
            "type": "object",
            "required": ["text", "language", "receipt_hash"],
            "properties": {
                "text": { "type": "string" },
                "language": { "type": "string" },
                "duration_seconds": { "type": "integer" },
                "provider_id": { "type": "string" },
                "model_id": { "type": "string" },
                "receipt_hash": { "type": "string" }
            }
        }),
        examples: vec![ardur_tool_registry::ToolExample {
            description: "Transcribe a short WAV note.".to_string(),
            args: serde_json::json!({
                "audio_base64": "UklGRgAAAApXQVZF",
                "format": "wav",
                "duration_seconds_upper_bound": 3
            }),
            output: serde_json::json!({
                "text": "hello world",
                "language": "en",
                "duration_seconds": 3,
                "provider_id": DEFAULT_PROVIDER_ID,
                "model_id": DEFAULT_MODEL,
                "receipt_hash": "..."
            }),
        }],
    }
}
