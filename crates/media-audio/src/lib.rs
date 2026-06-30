//! Audio transcription domain contracts for ARD-109 / plan §10.3.
//!
//! This crate freezes the Phase 1 type surface for file transcription,
//! streaming transcription, and diarization. It intentionally contains no
//! provider HTTP clients and no FFmpeg subprocesses; later phases can implement
//! those behind the closed [`TranscriptionProvider`] trait without changing the
//! request, cost, refusal, receipt, or transcript vocabulary.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use ardur_media_decode::AudioFormat;
use ardur_runtime::CapTokenRef;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

mod local_voice;
mod whisper;

pub use local_voice::{
    LocalSpeechToTextProvider, LocalSttConfig, LocalTextToSpeechProvider, LocalTtsConfig,
    TextToSpeechOutput, TextToSpeechProvider, TextToSpeechRequest, VoiceSpeakTool,
};
pub use whisper::{VoiceTranscribeTool, WhisperApiConfig, WhisperApiTranscriptionProvider};

/// Unix timestamp in milliseconds since the epoch.
pub type UnixTsMillis = u64;

macro_rules! string_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wrap a string-like value in this identifier type.
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Borrow the raw identifier string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

macro_rules! uuid_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Mint a fresh random identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

string_newtype!(
    /// Identifier of an audio transcription provider.
    AudioProviderId
);
string_newtype!(
    /// Identifier of a concrete audio model.
    AudioModelId
);
string_newtype!(
    /// Identifier of the mission authorizing an audio operation.
    MissionId
);
string_newtype!(
    /// Handle for audio bytes stored outside the transcript row.
    ArtifactBytesHandle
);
string_newtype!(
    /// Handle for a persisted audio artifact.
    AudioArtifactHandle
);
string_newtype!(
    /// Handle for a persisted transcript artifact.
    TranscriptHandle
);
string_newtype!(
    /// Hash of an audio byte stream.
    AudioHash
);
string_newtype!(
    /// Hash of a transcript byte stream.
    TranscriptHash
);
string_newtype!(
    /// Provider/model version pin used for drift detection.
    ModelVersionPin
);
string_newtype!(
    /// Hash of the redaction vocabulary used for an operation.
    VocabHash
);
string_newtype!(
    /// Hash of the receipt that proves an audio artifact or transcript.
    ReceiptHash
);
string_newtype!(
    /// Identifier of a streaming transcription session.
    StreamId
);

uuid_newtype!(
    /// Identifier of an audio artifact.
    ArtifactId
);
uuid_newtype!(
    /// Identifier of a transcript.
    TranscriptId
);
uuid_newtype!(
    /// Identifier of a transcript segment.
    SegmentId
);

/// The audio operation being authorized and costed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioVerb {
    /// One-shot audio file transcription.
    TranscribeFile,
    /// Live or long-running streaming transcription.
    TranscribeStream,
    /// Speaker segmentation applied to an existing transcript.
    ApplyDiarization,
}

/// Closed content-class taxonomy used by audio policy ceilings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentClass {
    /// No elevated handling required.
    Safe,
    /// Mildly sensitive or suggestive content.
    Suggestive,
    /// Content that requires stricter retention or disclosure handling.
    Sensitive,
    /// Adult content.
    Adult,
    /// Hateful or harassment-related content.
    Hateful,
    /// Content that policy forbids processing.
    Disallowed,
}

impl ContentClass {
    /// Whether this detected class is above an authorized ceiling.
    #[must_use]
    pub fn exceeds_ceiling(self, ceiling: Self) -> bool {
        self > ceiling
    }
}

/// BCP-47-ish language tag carried by requests and transcript metadata.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LanguageTag(String);

impl LanguageTag {
    /// Construct a language tag from a non-empty string.
    pub fn new(tag: impl Into<String>) -> Result<Self, AudioError> {
        let tag = tag.into();
        if tag.trim().is_empty() {
            return Err(AudioError::InvalidRequest(
                "language tag must be non-empty".to_string(),
            ));
        }
        Ok(Self(tag))
    }

    /// Borrow the raw language tag.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Local or persisted audio input accepted by file-mode transcription.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum AudioInput {
    /// Audio already persisted in the artifact store.
    Artifact {
        /// Artifact handle for stored bytes.
        handle: AudioArtifactHandle,
        /// Declared format for capability checks.
        format: AudioFormat,
    },
    /// Small inline audio bytes, only for bounded tests and tiny uploads.
    InlineBytes {
        /// Audio bytes.
        bytes: Vec<u8>,
        /// Declared format for capability checks.
        format: AudioFormat,
    },
    /// Workspace-local path. The future pipeline must canonicalize it before use.
    WorkspacePath {
        /// Path supplied by the caller.
        path: PathBuf,
        /// Declared format for capability checks.
        format: AudioFormat,
    },
}

impl AudioInput {
    /// Return the declared format carried by this input.
    #[must_use]
    pub fn format(&self) -> AudioFormat {
        match self {
            Self::Artifact { format, .. }
            | Self::InlineBytes { format, .. }
            | Self::WorkspacePath { format, .. } => *format,
        }
    }
}

/// Supported upstreams for streaming transcription sessions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum StreamUpstream {
    /// Microphone capture from a local device identifier.
    MicCapture {
        /// Local device identifier.
        device_id: String,
    },
    /// RTP source endpoint.
    RtpEndpoint {
        /// RTP URL or endpoint descriptor.
        url: String,
    },
    /// Existing WebRTC session identifier.
    WebRtcSession {
        /// WebRTC session id.
        session_id: String,
    },
}

/// Diarization mode for a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiarizationMode {
    /// Do not apply speaker segmentation.
    None,
    /// Use provider-native speaker segmentation.
    ProviderNative,
    /// Apply a local post-hoc diarization pass.
    PostHocLocal,
}

/// Export representation requested for transcript output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptFormat {
    /// Plain text transcript.
    Plaintext,
    /// SubRip subtitle format.
    Srt,
    /// WebVTT subtitle format.
    Vtt,
    /// Structured JSON transcript.
    Json,
}

/// Audio-operation scope carried by an authorized token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioScope {
    /// Verb the token permits.
    pub verb: AudioVerb,
    /// Provider the token permits.
    pub provider_id: AudioProviderId,
    /// Maximum duration this token permits.
    pub duration_seconds_ceiling: u32,
    /// Maximum content class this token permits.
    pub content_class_ceiling: ContentClass,
}

/// Capability token plus the parsed audio scope derived from it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedAudioToken {
    /// Runtime capability token reference.
    pub cap_token: CapTokenRef,
    /// Audio-specific attenuation scope.
    pub scope: AudioScope,
}

/// File-mode transcription request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscribeFileRequest {
    /// Provider to invoke.
    pub provider_id: AudioProviderId,
    /// Model to invoke.
    pub model_id: AudioModelId,
    /// Audio input to transcribe.
    pub input: AudioInput,
    /// Upper-bound duration for admission and capability checks.
    pub duration_seconds_upper_bound: u32,
    /// Optional source-language hint.
    pub language_hint: Option<LanguageTag>,
    /// Optional target language for provider-native translation.
    pub target_language: Option<LanguageTag>,
    /// Requested diarization mode.
    pub diarization: DiarizationMode,
    /// Optional speaker-count ceiling.
    pub max_speakers: Option<u8>,
    /// Requested transcript export format.
    pub export_format: TranscriptFormat,
    /// Mission id for audit linkage.
    pub mission_id: MissionId,
    /// Caller-observed request timestamp.
    pub requested_at: UnixTsMillis,
}

/// Streaming transcription request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscribeStreamRequest {
    /// Provider to invoke.
    pub provider_id: AudioProviderId,
    /// Model to invoke.
    pub model_id: AudioModelId,
    /// Live upstream source.
    pub upstream: StreamUpstream,
    /// Optional source-language hint.
    pub language_hint: Option<LanguageTag>,
    /// Expected upper-bound duration for admission.
    pub expected_duration_seconds_upper_bound: u32,
    /// Receipt sampling cadence in seconds. Valid range is 1..=60.
    pub sample_cadence_seconds: u16,
    /// Mission id for audit linkage.
    pub mission_id: MissionId,
    /// Caller-observed request timestamp.
    pub requested_at: UnixTsMillis,
}

impl TranscribeStreamRequest {
    /// Validate stream-specific invariants.
    pub fn validate(&self) -> Result<(), AudioError> {
        if !(1..=60).contains(&self.sample_cadence_seconds) {
            return Err(AudioError::InvalidRequest(
                "sample cadence must be in the range 1..=60 seconds".to_string(),
            ));
        }
        Ok(())
    }
}

/// Request to apply diarization to an existing transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiarizationRequest {
    /// Provider to invoke.
    pub provider_id: AudioProviderId,
    /// Model to invoke.
    pub model_id: AudioModelId,
    /// Transcript handle to update.
    pub transcript_handle: TranscriptHandle,
    /// Diarization mode. `None` is invalid for this request.
    pub mode: DiarizationMode,
    /// Optional speaker-count ceiling.
    pub max_speakers: Option<u8>,
    /// Mission id for audit linkage.
    pub mission_id: MissionId,
    /// Caller-observed request timestamp.
    pub requested_at: UnixTsMillis,
}

/// Borrowed request reference used by cost-envelope calculation.
#[derive(Clone, Copy, Debug)]
pub enum AudioRequestRef<'a> {
    /// File transcription request reference.
    File(&'a TranscribeFileRequest),
    /// Streaming transcription request reference.
    Stream(&'a TranscribeStreamRequest),
    /// Diarization request reference.
    Diarization(&'a DiarizationRequest),
}

/// Range of a transcript segment in seconds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SegmentRange {
    /// Start offset in seconds.
    pub start_seconds: f32,
    /// End offset in seconds.
    pub end_seconds: f32,
}

/// Speaker label attached by diarization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum SpeakerLabel {
    /// Speaker is unknown.
    Unknown,
    /// Stable numeric speaker id assigned by the diarization pass.
    SpeakerId(u8),
    /// Human-assigned speaker name.
    NamedSpeaker(String),
}

/// One transcript segment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    /// Segment id.
    pub segment_id: SegmentId,
    /// Start offset in seconds.
    pub start_seconds: f32,
    /// End offset in seconds.
    pub end_seconds: f32,
    /// Segment text.
    pub text: String,
    /// Segment-level language override, when it differs from the transcript.
    pub language: Option<LanguageTag>,
    /// Speaker label, populated by diarization.
    pub speaker: Option<SpeakerLabel>,
    /// Provider confidence in the range 0.0..=1.0.
    pub confidence: f32,
}

/// Complete transcription result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// Transcript id.
    pub transcript_id: TranscriptId,
    /// Ordered transcript segments.
    pub segments: Vec<TranscriptSegment>,
    /// Detected primary language.
    pub language_detected: LanguageTag,
    /// Per-range language overrides.
    pub language_per_segment: Vec<(SegmentRange, LanguageTag)>,
    /// Handle to the source audio artifact.
    pub source_audio_handle: AudioArtifactHandle,
    /// Audio duration in seconds.
    pub duration_seconds: u32,
    /// Provider that produced the transcript.
    pub provider_id: AudioProviderId,
    /// Model that produced the transcript.
    pub model_id: AudioModelId,
    /// Assigned content class.
    pub content_class: ContentClass,
    /// Number of redaction windows applied before provider submission.
    pub redaction_window_count: u32,
    /// Creation timestamp.
    pub created_at: UnixTsMillis,
    /// Receipt hash proving this transcript.
    pub receipt_hash: ReceiptHash,
}

/// Audio artifact metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioArtifact {
    /// Artifact id.
    pub artifact_id: ArtifactId,
    /// Provider associated with the artifact.
    pub provider_id: AudioProviderId,
    /// Model associated with the artifact.
    pub model_id: AudioModelId,
    /// Handle to stored bytes.
    pub bytes_handle: ArtifactBytesHandle,
    /// Audio format.
    pub format: AudioFormat,
    /// Duration in seconds.
    pub duration_seconds: u32,
    /// Sample rate.
    pub sample_rate_hz: u32,
    /// Channel count.
    pub channel_count: u8,
    /// Provenance metadata.
    pub provenance: AudioProvenance,
    /// Cost envelope that admitted the operation.
    pub cost_envelope: AudioCostEnvelope,
    /// Assigned content class.
    pub content_class: ContentClass,
    /// Creation timestamp.
    pub created_at: UnixTsMillis,
    /// Receipt hash proving this artifact.
    pub receipt_hash: ReceiptHash,
}

/// Provenance captured for an audio operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioProvenance {
    /// Hash of input audio bytes.
    pub input_audio_hash: AudioHash,
    /// Hash of output transcript bytes, when available.
    pub output_transcript_hash: Option<TranscriptHash>,
    /// Provider id.
    pub provider_id: AudioProviderId,
    /// Model id.
    pub model_id: AudioModelId,
    /// Optional model version pin.
    pub model_version_pin: Option<ModelVersionPin>,
    /// Detected language, when available.
    pub language_detected: Option<LanguageTag>,
    /// Number of redaction windows applied.
    pub redaction_window_count: u32,
    /// Hash of the redaction vocabulary used.
    pub redaction_vocab_hash: Option<VocabHash>,
    /// Whether diarization was applied.
    pub diarization_applied: bool,
    /// Speaker count after diarization.
    pub speaker_count: Option<u8>,
}

/// Audio-specific cost envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum AudioCostEnvelope {
    /// File transcription cost.
    File {
        /// Duration in seconds.
        duration_seconds: u32,
        /// Price per second in micro-USD.
        per_second_micro_usd: u64,
        /// Provider minimum charge.
        floor_micro_usd: u64,
        /// Additional diarization charge.
        diarization_overhead_micro_usd: u64,
    },
    /// Streaming transcription cost.
    Stream {
        /// Expected upper-bound duration in seconds.
        expected_duration_seconds_upper_bound: u32,
        /// Receipt sample cadence in seconds.
        sample_cadence_seconds: u16,
        /// Price per streamed second in micro-USD.
        per_second_micro_usd: u64,
        /// Connection minimum charge.
        connection_floor_micro_usd: u64,
        /// Charge attributed to each sampled receipt.
        per_sample_receipt_overhead_micro_usd: u64,
    },
    /// Diarization cost.
    Diarization {
        /// Segmented duration in seconds.
        segmented_duration_seconds: u32,
        /// Price per segmented second in micro-USD.
        per_second_micro_usd: u64,
        /// Provider minimum charge.
        floor_micro_usd: u64,
    },
}

impl AudioCostEnvelope {
    /// Deterministically compute the total micro-USD ceiling.
    #[must_use]
    pub fn total_micro_usd(&self) -> u64 {
        match self {
            Self::File {
                duration_seconds,
                per_second_micro_usd,
                floor_micro_usd,
                diarization_overhead_micro_usd,
            } => u64::from(*duration_seconds)
                .saturating_mul(*per_second_micro_usd)
                .saturating_add(*floor_micro_usd)
                .saturating_add(*diarization_overhead_micro_usd),
            Self::Stream {
                expected_duration_seconds_upper_bound,
                sample_cadence_seconds,
                per_second_micro_usd,
                connection_floor_micro_usd,
                per_sample_receipt_overhead_micro_usd,
            } => {
                let cadence = u32::from((*sample_cadence_seconds).max(1));
                let samples = expected_duration_seconds_upper_bound.div_ceil(cadence);
                u64::from(*expected_duration_seconds_upper_bound)
                    .saturating_mul(*per_second_micro_usd)
                    .saturating_add(*connection_floor_micro_usd)
                    .saturating_add(
                        u64::from(samples).saturating_mul(*per_sample_receipt_overhead_micro_usd),
                    )
            }
            Self::Diarization {
                segmented_duration_seconds,
                per_second_micro_usd,
                floor_micro_usd,
            } => u64::from(*segmented_duration_seconds)
                .saturating_mul(*per_second_micro_usd)
                .saturating_add(*floor_micro_usd),
        }
    }
}

/// Closed refusal taxonomy for audio operations.
#[derive(Clone, Debug, Error, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum AudioRefusalReason {
    /// The token did not carry the required audio scope.
    #[error("cap-token does not carry required audio scope")]
    CapTokenInsufficient {
        /// Scope required by the request.
        required_scope: AudioScope,
    },
    /// Requested verb/format/duration exceeds provider capabilities.
    #[error("requested audio capability exceeds provider surface: {requested}")]
    CapabilitySurfaceExceeded {
        /// Requested capability description.
        requested: String,
        /// Supported capability descriptions.
        supported: Vec<String>,
    },
    /// Detected content class exceeded the token or policy ceiling.
    #[error("content class ceiling exceeded")]
    ContentClassCeilingExceeded {
        /// Detected content class.
        detected: ContentClass,
        /// Authorized ceiling.
        ceiling: ContentClass,
    },
    /// Cost budget could not admit the projected envelope.
    #[error("audio cost budget exhausted")]
    CostBudgetExhausted {
        /// Remaining budget in micro-USD.
        remaining_micro_usd: i64,
        /// Required envelope in micro-USD.
        envelope_micro_usd: u64,
    },
    /// Request duration exceeded the authorized ceiling.
    #[error("audio duration ceiling exceeded")]
    DurationCeilingExceeded {
        /// Requested duration in seconds.
        requested: u32,
        /// Ceiling in seconds.
        ceiling: u32,
    },
    /// Workspace path-safety refused a local input.
    #[error("workspace scope refused local audio path: {path:?}")]
    WorkspaceScopeRefused {
        /// Refused path.
        path: PathBuf,
    },
    /// Provider-side moderation refused the operation.
    #[error("provider-side moderation refused audio")]
    ProviderSideModerationRefused {
        /// Provider id.
        provider_id: AudioProviderId,
        /// Provider error or moderation code.
        code: String,
    },
    /// Requested diarization is not supported by this provider.
    #[error("diarization unsupported by provider {provider_id}")]
    DiarizationUnsupported {
        /// Provider id.
        provider_id: AudioProviderId,
    },
    /// Streaming upstream was refused before provider invocation.
    #[error("stream upstream refused: {reason}")]
    StreamUpstreamRefused {
        /// Upstream descriptor.
        upstream: String,
        /// Refusal reason.
        reason: String,
    },
    /// Provider was unreachable.
    #[error("provider unreachable: {provider_id}")]
    ProviderUnreachable {
        /// Provider id.
        provider_id: AudioProviderId,
        /// Retry-after hint in milliseconds.
        retry_after_ms: Option<u64>,
    },
    /// Parent verification failed before the operation could run.
    #[error("parent verification failed: {reason}")]
    ParentVerificationFailed {
        /// Verification failure reason.
        reason: String,
    },
}

/// Top-level audio error.
#[derive(Debug, Error)]
pub enum AudioError {
    /// Request failed local validation.
    #[error("invalid audio request: {0}")]
    InvalidRequest(String),
    /// Operation was refused by policy/capability/admission.
    #[error("audio operation refused: {0}")]
    Refused(#[from] AudioRefusalReason),
    /// Provider adapter failed after admission.
    #[error("audio provider error: {0}")]
    Provider(String),
}

/// Result of applying diarization to a transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiarizationOutcome {
    /// Transcript id that was updated.
    pub transcript_id: TranscriptId,
    /// Number of segments updated.
    pub segments_updated: u32,
    /// Number of speakers found.
    pub speaker_count: u8,
    /// Cost envelope used for the operation.
    pub cost_envelope: AudioCostEnvelope,
    /// Receipt hash proving this diarization pass.
    pub receipt_hash: ReceiptHash,
}

/// Declared provider capability surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptionCapabilitySurface {
    /// Supported audio verbs.
    pub supported_verbs: BTreeSet<AudioVerb>,
    /// Supported input formats.
    pub supported_input_formats: BTreeSet<AudioFormat>,
    /// Maximum accepted file duration in seconds.
    pub max_duration_seconds_per_file: u32,
    /// Supported languages.
    pub supported_languages: BTreeSet<LanguageTag>,
    /// Whether provider-native diarization is available.
    pub supports_diarization_native: bool,
    /// Whether streaming is available.
    pub supports_streaming: bool,
    /// Maximum concurrent streams allowed.
    pub max_concurrent_streams: u16,
    /// Optional model-version pin.
    pub model_version_pinned: Option<ModelVersionPin>,
}

impl TranscriptionCapabilitySurface {
    /// Validate whether this surface can accept a file transcription request.
    pub fn check_file_request(
        &self,
        request: &TranscribeFileRequest,
    ) -> Result<(), AudioRefusalReason> {
        if !self.supported_verbs.contains(&AudioVerb::TranscribeFile) {
            return Err(AudioRefusalReason::CapabilitySurfaceExceeded {
                requested: "verb=transcribe_file".to_string(),
                supported: self.supported_verb_names(),
            });
        }
        if !self
            .supported_input_formats
            .contains(&request.input.format())
        {
            return Err(AudioRefusalReason::CapabilitySurfaceExceeded {
                requested: format!("format={:?}", request.input.format()),
                supported: self.supported_format_names(),
            });
        }
        if request.duration_seconds_upper_bound > self.max_duration_seconds_per_file {
            return Err(AudioRefusalReason::DurationCeilingExceeded {
                requested: request.duration_seconds_upper_bound,
                ceiling: self.max_duration_seconds_per_file,
            });
        }
        if request.diarization == DiarizationMode::ProviderNative
            && !self.supports_diarization_native
        {
            return Err(AudioRefusalReason::DiarizationUnsupported {
                provider_id: request.provider_id.clone(),
            });
        }
        Ok(())
    }

    fn supported_verb_names(&self) -> Vec<String> {
        self.supported_verbs
            .iter()
            .map(|verb| format!("{verb:?}"))
            .collect()
    }

    fn supported_format_names(&self) -> Vec<String> {
        self.supported_input_formats
            .iter()
            .map(|format| format!("{format:?}"))
            .collect()
    }
}

/// Handle returned when a streaming transcription session opens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamHandle {
    /// Stream id.
    pub stream_id: StreamId,
    /// Provider serving the stream.
    pub provider_id: AudioProviderId,
    /// Model serving the stream.
    pub model_id: AudioModelId,
    /// Receipt sample cadence in seconds.
    pub sample_cadence_seconds: u16,
    /// Open timestamp.
    pub opened_at: UnixTsMillis,
}

/// Minimal media-provider supertrait until §10.0 lands as its own crate.
pub trait MediaProvider: Send + Sync {
    /// Provider id used for registry lookup and receipts.
    fn media_provider_id(&self) -> &AudioProviderId;
}

/// Closed transcription provider trait.
#[async_trait]
pub trait TranscriptionProvider: MediaProvider + private::Sealed {
    /// Transcribe a single audio input into a transcript.
    async fn transcribe_file(
        &self,
        token: &AuthorizedAudioToken,
        request: TranscribeFileRequest,
    ) -> Result<Transcript, AudioError>;

    /// Open a streaming transcription session.
    async fn transcribe_stream(
        &self,
        token: &AuthorizedAudioToken,
        request: TranscribeStreamRequest,
    ) -> Result<StreamHandle, AudioError>;

    /// Apply speaker segmentation to an existing transcript.
    async fn apply_diarization(
        &self,
        token: &AuthorizedAudioToken,
        request: DiarizationRequest,
    ) -> Result<DiarizationOutcome, AudioError>;

    /// Compute the pre-admission cost envelope for a request.
    fn cost_envelope(&self, verb: AudioVerb, request: AudioRequestRef<'_>) -> AudioCostEnvelope;

    /// Return the provider's declared capability surface.
    fn capability_surface(&self) -> TranscriptionCapabilitySurface;

    /// Detect or normalize the language tag for a transcript.
    fn detect_language(&self, transcript: &Transcript) -> LanguageTag;
}

mod private {
    pub trait Sealed {}
}

/// Receipt-event vocabulary for audio transcription.
pub mod receipt_events {
    /// Plan-native event name for file transcription success.
    pub const PLAN_AUDIO_TRANSCRIBED_V1: &str = "audio.transcribed.v1";
    /// Plan-native event name for sampled stream transcription.
    pub const PLAN_AUDIO_STREAM_TRANSCRIBED_V1: &str = "audio.stream.transcribed.v1";
    /// Plan-native event name for diarization success.
    pub const PLAN_AUDIO_DIARIZATION_APPLIED_V1: &str = "audio.diarization.applied.v1";

    /// Receipt-grammar-safe file transcription success verb.
    pub const AUDIO_FILE_TRANSCRIBED_SUCCEEDED_V1: &str = "audio.file_transcribed.succeeded.v1";
    /// Receipt-grammar-safe sampled stream transcription success verb.
    pub const AUDIO_STREAM_TRANSCRIBED_SUCCEEDED_V1: &str = "audio.stream_transcribed.succeeded.v1";
    /// Receipt-grammar-safe diarization success verb.
    pub const AUDIO_DIARIZATION_APPLIED_SUCCEEDED_V1: &str =
        "audio.diarization_applied.succeeded.v1";
    /// Receipt-grammar-safe audio redaction success verb.
    pub const AUDIO_REDACTION_APPLIED_SUCCEEDED_V1: &str = "audio.redaction_applied.succeeded.v1";
    /// Receipt-grammar-safe language detection success verb.
    pub const AUDIO_LANGUAGE_DETECTED_SUCCEEDED_V1: &str = "audio.language_detected.succeeded.v1";
    /// Receipt-grammar-safe audio operation refusal verb.
    pub const AUDIO_OPERATION_REFUSED_V1: &str = "audio.operation.refused.v1";
    /// Receipt-grammar-safe cost reconciliation success verb.
    pub const AUDIO_COST_RECONCILED_SUCCEEDED_V1: &str = "audio.cost_reconciled.succeeded.v1";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt_events::*;

    #[test]
    fn file_cost_includes_floor_and_diarization_overhead() {
        let envelope = AudioCostEnvelope::File {
            duration_seconds: 30,
            per_second_micro_usd: 10,
            floor_micro_usd: 100,
            diarization_overhead_micro_usd: 25,
        };

        assert_eq!(envelope.total_micro_usd(), 425);
    }

    #[test]
    fn stream_cost_counts_sampled_receipts_with_ceiling_division() {
        let envelope = AudioCostEnvelope::Stream {
            expected_duration_seconds_upper_bound: 61,
            sample_cadence_seconds: 10,
            per_second_micro_usd: 2,
            connection_floor_micro_usd: 100,
            per_sample_receipt_overhead_micro_usd: 5,
        };

        assert_eq!(envelope.total_micro_usd(), 61 * 2 + 100 + 7 * 5);
    }

    #[test]
    fn content_class_ceiling_uses_closed_ordering() {
        assert!(!ContentClass::Sensitive.exceeds_ceiling(ContentClass::Sensitive));
        assert!(ContentClass::Adult.exceeds_ceiling(ContentClass::Sensitive));
        assert!(ContentClass::Disallowed.exceeds_ceiling(ContentClass::Hateful));
    }

    #[test]
    fn capability_surface_rejects_unsupported_format_before_provider_call() {
        let surface = TranscriptionCapabilitySurface {
            supported_verbs: BTreeSet::from([AudioVerb::TranscribeFile]),
            supported_input_formats: BTreeSet::from([AudioFormat::Wav]),
            max_duration_seconds_per_file: 60,
            supported_languages: BTreeSet::new(),
            supports_diarization_native: false,
            supports_streaming: false,
            max_concurrent_streams: 0,
            model_version_pinned: None,
        };
        let request = TranscribeFileRequest {
            provider_id: AudioProviderId::new("mock"),
            model_id: AudioModelId::new("mock-transcribe"),
            input: AudioInput::WorkspacePath {
                path: PathBuf::from("voice.mp3"),
                format: AudioFormat::Mp3,
            },
            duration_seconds_upper_bound: 30,
            language_hint: None,
            target_language: None,
            diarization: DiarizationMode::None,
            max_speakers: None,
            export_format: TranscriptFormat::Json,
            mission_id: MissionId::new("mission"),
            requested_at: 1,
        };

        assert!(matches!(
            surface.check_file_request(&request),
            Err(AudioRefusalReason::CapabilitySurfaceExceeded { .. })
        ));
    }

    #[test]
    fn stream_request_validates_receipt_sample_cadence() {
        let request = TranscribeStreamRequest {
            provider_id: AudioProviderId::new("mock"),
            model_id: AudioModelId::new("mock-transcribe"),
            upstream: StreamUpstream::MicCapture {
                device_id: "default".to_string(),
            },
            language_hint: None,
            expected_duration_seconds_upper_bound: 60,
            sample_cadence_seconds: 0,
            mission_id: MissionId::new("mission"),
            requested_at: 1,
        };

        assert!(matches!(
            request.validate(),
            Err(AudioError::InvalidRequest(_))
        ));
    }

    #[test]
    fn normalized_receipt_events_match_current_receipt_grammar() {
        for verb in [
            AUDIO_FILE_TRANSCRIBED_SUCCEEDED_V1,
            AUDIO_STREAM_TRANSCRIBED_SUCCEEDED_V1,
            AUDIO_DIARIZATION_APPLIED_SUCCEEDED_V1,
            AUDIO_REDACTION_APPLIED_SUCCEEDED_V1,
            AUDIO_LANGUAGE_DETECTED_SUCCEEDED_V1,
            AUDIO_OPERATION_REFUSED_V1,
            AUDIO_COST_RECONCILED_SUCCEEDED_V1,
        ] {
            ardur_receipt::VerbObject::new(verb).expect("audio receipt verb is grammar-safe");
        }
    }

    #[test]
    fn plan_native_event_names_are_kept_for_traceability() {
        assert_eq!(PLAN_AUDIO_TRANSCRIBED_V1, "audio.transcribed.v1");
        assert_eq!(
            PLAN_AUDIO_STREAM_TRANSCRIBED_V1,
            "audio.stream.transcribed.v1"
        );
        assert_eq!(
            PLAN_AUDIO_DIARIZATION_APPLIED_V1,
            "audio.diarization.applied.v1"
        );
    }
}
