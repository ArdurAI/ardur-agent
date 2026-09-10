//! Video generate/describe/analyze domain contracts for plan §10.2.
//!
//! This crate freezes the Phase 1 type surface for text-to-video generation,
//! per-frame describe, and action/scene/transcript analyze. It ships one
//! concrete adapter — [`GeminiVideoAnalyzeProvider`] — that calls a
//! vision-capable multimodal API directly on whole-video bytes, and exposes all
//! three verbs as tools:
//!
//! - [`VideoAnalyzeTool`] (`video.analyze`) — action recognition, scene
//!   segmentation, and on-screen-text OCR, fully wired against Gemini.
//! - [`VideoDescribeTool`] (`video.describe`) — a whole-video scene
//!   description. True per-frame describe (FFmpeg frame sampling + §10.1
//!   delegation) is a forward-ref, so `sample_rate_fps` shapes only the scope
//!   and cost today; the summary carries the description.
//! - [`VideoGenerateTool`] (`video.generate`) — default-deny (tenant opt-in +
//!   per-cap-token duration ceiling). The shipped adapter does not generate, so
//!   an opted-in request surfaces the provider's forward-ref refusal; the gate,
//!   cost envelope, and receipt discipline are the tool's contribution until a
//!   Sora/Runway/Veo/Pika adapter lands.
//!
//! Every verb is cap-token-scoped ([`AuthorizedVideoToken`]), cost-gated
//! ([`VideoCostEnvelope`]), and receipt-recorded ([`receipt_events`]).
//! Provider-returned text is untrusted content from an external video, so the
//! describe and analyze tools scan it with `ardur-injection-defense` before it
//! re-enters agent context.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use ardur_runtime::CapTokenRef;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

mod gemini_video_in;

pub use gemini_video_in::{
    GeminiVideoAnalyzeProvider, GeminiVideoConfig, VideoAnalyzeTool, VideoDescribeTool,
    VideoGenerateTool,
};

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
    /// Identifier of a video provider (Sora, Runway, Veo, Pika, Gemini, ...).
    VideoProviderId
);
string_newtype!(
    /// Identifier of a concrete video model.
    VideoModelId
);
string_newtype!(
    /// Identifier of the mission authorizing a video operation.
    MissionId
);
string_newtype!(
    /// Handle for video bytes stored outside the artifact row.
    ArtifactBytesHandle
);
string_newtype!(
    /// Handle for a persisted video artifact.
    VideoArtifactHandle
);
string_newtype!(
    /// Handle for a per-frame image artifact (bridges to §10.1).
    ImageArtifactHandle
);
string_newtype!(
    /// Hash of a video byte stream.
    VideoHash
);
string_newtype!(
    /// Hash of a text prompt.
    PromptHash
);
string_newtype!(
    /// Provider/model version pin used for drift detection.
    ModelVersionPin
);
string_newtype!(
    /// Hash of the receipt that proves a video artifact or outcome.
    ReceiptHash
);
string_newtype!(
    /// Identifier of an async generation job on the provider side.
    ProviderJobId
);

uuid_newtype!(
    /// Identifier of a video artifact.
    ArtifactId
);
uuid_newtype!(
    /// Identifier of a sampled frame.
    FrameId
);

/// The video operation being authorized and costed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoVerb {
    /// Text-to-video (optionally image-conditioned) generation.
    Generate,
    /// Per-frame describe at an operator-declared sample rate.
    Describe,
    /// Action recognition, scene segmentation, OCR, or transcript extraction.
    Analyze,
}

/// Closed content-class taxonomy shared with the §10.1 image surface.
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
    pub fn new(tag: impl Into<String>) -> Result<Self, VideoError> {
        let tag = tag.into();
        if tag.trim().is_empty() {
            return Err(VideoError::InvalidRequest(
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

/// Video container/codec format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoFormat {
    /// MPEG-4 container, H.264 video.
    Mp4H264,
    /// MPEG-4 container, H.265/HEVC video.
    Mp4H265,
    /// WebM container, VP9 video.
    WebmVp9,
    /// QuickTime container.
    Mov,
}

impl VideoFormat {
    /// Return the canonical MIME type used for provider uploads.
    #[must_use]
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Mp4H264 | Self::Mp4H265 => "video/mp4",
            Self::WebmVp9 => "video/webm",
            Self::Mov => "video/quicktime",
        }
    }

    /// Parse a common file extension into a [`VideoFormat`].
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .as_str()
        {
            "mp4" | "m4v" => Some(Self::Mp4H264),
            "hevc" => Some(Self::Mp4H265),
            "webm" => Some(Self::WebmVp9),
            "mov" => Some(Self::Mov),
            _ => None,
        }
    }
}

/// Target resolution tier for generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoResolution {
    /// 480p.
    R480p,
    /// 720p.
    R720p,
    /// 1080p.
    R1080p,
    /// 4K.
    R4k,
}

impl VideoResolution {
    /// Basis-points cost multiplier applied by [`VideoCostEnvelope`] pricing
    /// tables (10000 = 1.0x, relative to 720p).
    #[must_use]
    pub fn cost_multiplier_bps(self) -> u32 {
        match self {
            Self::R480p => 6_000,
            Self::R720p => 10_000,
            Self::R1080p => 18_000,
            Self::R4k => 40_000,
        }
    }
}

/// Output aspect ratio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AspectRatio {
    /// Widescreen landscape.
    Wide16x9,
    /// Vertical / portrait.
    Tall9x16,
    /// Square.
    Square1x1,
    /// Standard-definition landscape.
    Standard4x3,
}

/// Local or persisted video input accepted by describe/analyze.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum VideoInput {
    /// Video already persisted in the artifact store.
    Artifact {
        /// Artifact handle for stored bytes.
        handle: VideoArtifactHandle,
        /// Declared format for capability checks.
        format: VideoFormat,
    },
    /// Small inline video bytes, only for bounded tests and tiny uploads.
    InlineBytes {
        /// Video bytes.
        bytes: Vec<u8>,
        /// Declared format for capability checks.
        format: VideoFormat,
    },
    /// Workspace-local path. The future pipeline must canonicalize it before
    /// use.
    WorkspacePath {
        /// Path supplied by the caller.
        path: PathBuf,
        /// Declared format for capability checks.
        format: VideoFormat,
    },
}

impl VideoInput {
    /// Return the declared format carried by this input.
    #[must_use]
    pub fn format(&self) -> VideoFormat {
        match self {
            Self::Artifact { format, .. }
            | Self::InlineBytes { format, .. }
            | Self::WorkspacePath { format, .. } => *format,
        }
    }
}

/// The analyze sub-objective, per §10.2 architecture.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum VideoAnalyzeObjective {
    /// Recognise activity classes with per-segment temporal bounds.
    ActionRecognition,
    /// Partition the video into scene cuts with a representative frame each.
    SceneSegmentation,
    /// Extract on-screen text (OCR) with temporal bounds.
    OnScreenTextOcr,
    /// Delegate to §10.3 STT over the audio track.
    ExtractTranscript {
        /// Whether provider-native diarization should be requested.
        diarize: bool,
        /// Optional language pin.
        language_tag: Option<LanguageTag>,
    },
}

/// Video-operation scope carried by an authorized token.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoScope {
    /// Verb the token permits.
    pub verb: VideoVerb,
    /// Provider the token permits.
    pub provider_id: VideoProviderId,
    /// Maximum content class this token permits.
    pub content_class_ceiling: ContentClass,
    /// Maximum duration (seconds) this token permits.
    pub duration_seconds_ceiling: u32,
    /// Maximum describe sample rate (frames/sec) this token permits. Only
    /// meaningful for [`VideoVerb::Describe`].
    pub sample_rate_fps_ceiling: Option<f32>,
}

/// Capability token plus the parsed video scope derived from it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuthorizedVideoToken {
    /// Runtime capability token reference.
    pub cap_token: CapTokenRef,
    /// Video-specific attenuation scope.
    pub scope: VideoScope,
}

/// Text-to-video (optionally image-conditioned) generation request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoGenerateRequest {
    /// Provider to invoke.
    pub provider_id: VideoProviderId,
    /// Model to invoke.
    pub model_id: VideoModelId,
    /// Text prompt.
    pub prompt: String,
    /// Optional negative prompt.
    pub negative_prompt: Option<String>,
    /// Optional reference image for image-to-video conditioning.
    pub reference_image: Option<ImageArtifactHandle>,
    /// Requested duration in seconds.
    pub duration_seconds: u16,
    /// Requested resolution tier.
    pub resolution: VideoResolution,
    /// Requested frames per second.
    pub fps: u8,
    /// Requested aspect ratio.
    pub aspect_ratio: AspectRatio,
    /// Optional style hint.
    pub style: Option<String>,
    /// Optional deterministic seed.
    pub seed: Option<u64>,
    /// Mission id for audit linkage.
    pub mission_id: MissionId,
    /// Caller-observed request timestamp.
    pub requested_at: UnixTsMillis,
}

/// Per-frame describe request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoDescribeRequest {
    /// Provider to invoke.
    pub provider_id: VideoProviderId,
    /// Model to invoke.
    pub model_id: VideoModelId,
    /// Video input to describe.
    pub input: VideoInput,
    /// Sample rate, frames per second of video to sample.
    pub sample_rate_fps: SampleRateFps,
    /// Whether to emit an aggregated scene-summary second pass.
    pub include_scene_summary: bool,
    /// Language tag for the returned description.
    pub language_tag: LanguageTag,
    /// Mission id for audit linkage.
    pub mission_id: MissionId,
    /// Caller-observed request timestamp.
    pub requested_at: UnixTsMillis,
}

/// A validated positive sample rate in frames-per-second.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SampleRateFps(f32);

impl SampleRateFps {
    /// Construct a sample rate, rejecting non-finite or non-positive values.
    pub fn new(value: f32) -> Result<Self, VideoError> {
        if !value.is_finite() || value <= 0.0 {
            return Err(VideoError::InvalidRequest(
                "sample_rate_fps must be a finite positive number".to_string(),
            ));
        }
        Ok(Self(value))
    }

    /// Borrow the raw frames-per-second value.
    #[must_use]
    pub fn value(self) -> f32 {
        self.0
    }
}

/// Analyze request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoAnalyzeRequest {
    /// Provider to invoke.
    pub provider_id: VideoProviderId,
    /// Model to invoke.
    pub model_id: VideoModelId,
    /// Video input to analyze.
    pub input: VideoInput,
    /// Analyze sub-objective.
    pub objective: VideoAnalyzeObjective,
    /// Upper-bound duration for admission and capability checks.
    pub duration_seconds_upper_bound: u32,
    /// Mission id for audit linkage.
    pub mission_id: MissionId,
    /// Caller-observed request timestamp.
    pub requested_at: UnixTsMillis,
}

/// Borrowed request reference used by cost-envelope calculation.
#[derive(Clone, Copy, Debug)]
pub enum VideoRequestRef<'a> {
    /// Generate request reference.
    Generate(&'a VideoGenerateRequest),
    /// Describe request reference.
    Describe(&'a VideoDescribeRequest),
    /// Analyze request reference.
    Analyze(&'a VideoAnalyzeRequest),
}

/// One action-recognition segment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionSegment {
    /// Segment start, in seconds.
    pub start_seconds: f32,
    /// Segment end, in seconds.
    pub end_seconds: f32,
    /// Recognised action label.
    pub action_label: String,
    /// Provider confidence in the range 0.0..=1.0.
    pub confidence: f32,
}

/// One scene-segmentation segment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneSegment {
    /// Segment start, in seconds.
    pub start_seconds: f32,
    /// Segment end, in seconds.
    pub end_seconds: f32,
    /// Representative frame for this scene, when a frame sampler is
    /// available.
    pub representative_frame: Option<ImageArtifactHandle>,
    /// Human-readable scene description.
    pub description: String,
}

/// One on-screen-text OCR segment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OcrTemporalSegment {
    /// Segment start, in seconds.
    pub start_seconds: f32,
    /// Segment end, in seconds.
    pub end_seconds: f32,
    /// Recognised on-screen text.
    pub text: String,
}

/// One transcript segment (mirrors §10.3's shape without a hard crate
/// dependency, since §10.3 transcript delegation is a forward-ref here).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    /// Segment start, in seconds.
    pub start_seconds: f32,
    /// Segment end, in seconds.
    pub end_seconds: f32,
    /// Segment text.
    pub text: String,
    /// Speaker label, when diarization ran.
    pub speaker_tag: Option<String>,
}

/// The structured result of an [`VideoAnalyzeObjective`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum VideoAnalyzeStructured {
    /// Action-recognition segments.
    ActionRecognition {
        /// Recognised action segments.
        segments: Vec<ActionSegment>,
    },
    /// Scene-segmentation segments.
    SceneSegmentation {
        /// Scene segments.
        scenes: Vec<SceneSegment>,
    },
    /// On-screen-text OCR segments.
    OnScreenTextOcr {
        /// OCR segments.
        segments: Vec<OcrTemporalSegment>,
    },
    /// Transcript extraction, delegated to §10.3.
    ExtractTranscript {
        /// Receipt hash of the §10.3 `audio.transcribed.v1` receipt, when the
        /// delegation succeeded.
        transcript_receipt_hash: Option<ReceiptHash>,
        /// Transcript segments.
        transcript_segments: Vec<TranscriptSegment>,
    },
}

/// Output of an `analyze` call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoAnalyzeOutput {
    /// Provider that produced the result.
    pub provider_id: VideoProviderId,
    /// Model that produced the result.
    pub model_id: VideoModelId,
    /// Objective that was analyzed.
    pub objective: VideoAnalyzeObjective,
    /// Structured result.
    pub structured: VideoAnalyzeStructured,
    /// Assigned content class.
    pub content_class: ContentClass,
    /// Cost envelope used for the operation.
    pub cost_envelope: VideoCostEnvelope,
    /// Creation timestamp.
    pub created_at: UnixTsMillis,
    /// Receipt hash proving this analyze outcome.
    pub receipt_hash: ReceiptHash,
}

/// One per-frame describe entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameDescribeEntry {
    /// Zero-based frame index within the sampled sequence.
    pub frame_index: u32,
    /// Timestamp of this frame, in seconds.
    pub timestamp_seconds: f32,
    /// Handle to the sampled frame image artifact.
    pub image_artifact: ImageArtifactHandle,
    /// Per-frame description text (from the §10.1 delegation).
    pub description: String,
    /// Receipt hash of the chained `image.described.v1` receipt.
    pub describe_receipt_hash: ReceiptHash,
}

/// Output of a `describe` call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoDescribeOutput {
    /// Per-frame describe entries.
    pub per_frame: Vec<FrameDescribeEntry>,
    /// Optional aggregated scene summary.
    pub scene_summary: Option<String>,
    /// Language tag of the returned description.
    pub language_tag: LanguageTag,
    /// Assigned content class.
    pub content_class: ContentClass,
    /// Cost envelope used for the operation.
    pub cost_envelope: VideoCostEnvelope,
    /// Creation timestamp.
    pub created_at: UnixTsMillis,
    /// Receipt hash proving this describe outcome.
    pub receipt_hash: ReceiptHash,
}

/// Video artifact metadata (a generated or provided video, once persisted).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoArtifact {
    /// Artifact id.
    pub artifact_id: ArtifactId,
    /// Provider associated with the artifact.
    pub provider_id: VideoProviderId,
    /// Model associated with the artifact.
    pub model_id: VideoModelId,
    /// Handle to stored bytes.
    pub bytes_handle: ArtifactBytesHandle,
    /// Video format.
    pub format: VideoFormat,
    /// Duration in seconds.
    pub duration_seconds: u32,
    /// Frames per second.
    pub fps: u8,
    /// Width in pixels.
    pub width_px: u32,
    /// Height in pixels.
    pub height_px: u32,
    /// Whether this artifact has an audio track.
    pub has_audio_track: bool,
    /// Provenance metadata.
    pub provenance: VideoProvenance,
    /// Cost envelope that admitted the operation.
    pub cost_envelope: VideoCostEnvelope,
    /// Assigned content class.
    pub content_class: ContentClass,
    /// Creation timestamp.
    pub created_at: UnixTsMillis,
    /// Retention deadline; defaults to `created_at + 7 days`.
    pub retention_until: UnixTsMillis,
    /// Receipt hash proving this artifact.
    pub receipt_hash: ReceiptHash,
}

/// Default video retention floor, in seconds (7 days).
pub const DEFAULT_RETENTION_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Provenance captured for a video operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoProvenance {
    /// Hash of the input prompt, when generation was prompt-driven.
    pub input_prompt_hash: Option<PromptHash>,
    /// Hash of the reference image, when image-to-video conditioning was
    /// used.
    pub reference_image_hash: Option<VideoHash>,
    /// Deterministic seed, when supplied.
    pub seed: Option<u64>,
    /// Model id.
    pub model_id: VideoModelId,
    /// Provider id.
    pub provider_id: VideoProviderId,
    /// Optional model version pin.
    pub model_version_pin: Option<ModelVersionPin>,
    /// Provider job id, when the provider returned an async job handle.
    pub provider_job_id: Option<ProviderJobId>,
}

/// Video-specific cost envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum VideoCostEnvelope {
    /// Text-to-video generation cost.
    Generate {
        /// Requested duration in seconds.
        duration_seconds: u32,
        /// Base price per second, in micro-USD.
        per_second_micro_usd: u64,
        /// Resolution multiplier, in basis points (10000 = 1.0x).
        resolution_multiplier_bps: u32,
        /// FPS multiplier, in basis points (10000 = 1.0x).
        fps_multiplier_bps: u32,
        /// Provider minimum charge.
        floor_micro_usd: u64,
    },
    /// Per-frame describe cost.
    Describe {
        /// Number of sampled frames.
        sampled_frame_count: u32,
        /// Per-frame describe price, summing the §10.1 describe envelopes,
        /// in micro-USD.
        per_frame_describe_micro_usd: u64,
        /// Additional scene-summary charge (0 if not requested).
        scene_summary_micro_usd: u64,
        /// FFmpeg subprocess decode overhead, in micro-USD.
        decode_overhead_micro_usd: u64,
        /// Provider minimum charge.
        floor_micro_usd: u64,
    },
    /// Analyze cost.
    Analyze {
        /// Duration in seconds.
        duration_seconds: u32,
        /// Provider-declared per-second price, in micro-USD.
        per_second_analyze_micro_usd: u64,
        /// Per-objective constant overhead, in micro-USD.
        objective_overhead_micro_usd: u64,
        /// §10.3 delegation charge (0 unless `ExtractTranscript`).
        transcript_delegate_micro_usd: u64,
        /// Provider minimum charge.
        floor_micro_usd: u64,
    },
}

impl VideoCostEnvelope {
    /// Deterministically compute the total micro-USD ceiling.
    #[must_use]
    pub fn total_micro_usd(&self) -> u64 {
        match self {
            Self::Generate {
                duration_seconds,
                per_second_micro_usd,
                resolution_multiplier_bps,
                fps_multiplier_bps,
                floor_micro_usd,
            } => {
                let base = u64::from(*duration_seconds).saturating_mul(*per_second_micro_usd);
                let with_resolution = base
                    .saturating_mul(u64::from(*resolution_multiplier_bps))
                    .saturating_div(10_000);
                let with_fps = with_resolution
                    .saturating_mul(u64::from(*fps_multiplier_bps))
                    .saturating_div(10_000);
                with_fps.saturating_add(*floor_micro_usd)
            }
            Self::Describe {
                sampled_frame_count,
                per_frame_describe_micro_usd,
                scene_summary_micro_usd,
                decode_overhead_micro_usd,
                floor_micro_usd,
            } => u64::from(*sampled_frame_count)
                .saturating_mul(*per_frame_describe_micro_usd)
                .saturating_add(*scene_summary_micro_usd)
                .saturating_add(*decode_overhead_micro_usd)
                .saturating_add(*floor_micro_usd),
            Self::Analyze {
                duration_seconds,
                per_second_analyze_micro_usd,
                objective_overhead_micro_usd,
                transcript_delegate_micro_usd,
                floor_micro_usd,
            } => u64::from(*duration_seconds)
                .saturating_mul(*per_second_analyze_micro_usd)
                .saturating_add(*objective_overhead_micro_usd)
                .saturating_add(*transcript_delegate_micro_usd)
                .saturating_add(*floor_micro_usd),
        }
    }
}

/// Closed refusal taxonomy for video operations.
#[derive(Clone, Debug, Error, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum VideoRefusalReason {
    /// The token did not carry the required video scope.
    #[error("cap-token does not carry required video scope")]
    CapTokenInsufficient {
        /// Scope required by the request.
        required_scope: VideoScope,
    },
    /// Requested verb/format/resolution exceeds provider capabilities.
    #[error("requested video capability exceeds provider surface: {requested}")]
    CapabilitySurfaceExceeded {
        /// Requested capability description.
        requested: String,
        /// Supported capability descriptions.
        supported: Vec<String>,
    },
    /// Requested duration exceeded the authorized ceiling.
    #[error("video duration ceiling exceeded")]
    DurationCeilingExceeded {
        /// Requested duration in seconds.
        requested: u32,
        /// Ceiling in seconds.
        ceiling: u32,
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
    #[error("video cost budget exhausted")]
    CostBudgetExhausted {
        /// Remaining budget in micro-USD.
        remaining_micro_usd: i64,
        /// Required envelope in micro-USD.
        envelope_micro_usd: u64,
    },
    /// Workspace path-safety refused a local input.
    #[error("workspace scope refused local video path: {path:?}")]
    WorkspaceScopeRefused {
        /// Refused path.
        path: PathBuf,
    },
    /// Provider-side moderation refused the operation.
    #[error("provider-side moderation refused video")]
    ProviderSideModerationRefused {
        /// Provider id.
        provider_id: VideoProviderId,
        /// Provider error or moderation code.
        code: String,
    },
    /// The FFmpeg-backed frame sampler failed.
    #[error("frame sampler failed: {reason}")]
    FrameSamplerFailed {
        /// Failure reason.
        reason: String,
    },
    /// §10.3 transcript delegation failed.
    #[error("transcript delegation failed: {underlying}")]
    TranscriptDelegationFailed {
        /// Underlying failure description.
        underlying: String,
    },
    /// Provider was unreachable.
    #[error("provider unreachable: {provider_id}")]
    ProviderUnreachable {
        /// Provider id.
        provider_id: VideoProviderId,
        /// Retry-after hint in milliseconds.
        retry_after_ms: Option<u64>,
    },
    /// Parent verification failed before the operation could run.
    #[error("parent verification failed: {reason}")]
    ParentVerificationFailed {
        /// Verification failure reason.
        reason: String,
    },
    /// Tenant has not opted in to video generation.
    #[error("tenant has not opted in to video generation")]
    TenantOptInRequired {
        /// Tenant identifier.
        tenant_id: String,
    },
    /// The caller-supplied content was refused after an injection-defense
    /// scan of provider-returned text.
    #[error("provider-returned content blocked by injection defense: {reason}")]
    InjectionDefenseBlocked {
        /// Block reason surfaced by the filter.
        reason: String,
    },
}

/// Top-level video error.
#[derive(Debug, Error)]
pub enum VideoError {
    /// Request failed local validation.
    #[error("invalid video request: {0}")]
    InvalidRequest(String),
    /// Operation was refused by policy/capability/admission.
    #[error("video operation refused: {0}")]
    Refused(#[from] VideoRefusalReason),
    /// Provider adapter failed after admission.
    #[error("video provider error: {0}")]
    Provider(String),
}

/// Whether a provider's job model is synchronous or asynchronous.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsyncJobModel {
    /// Provider returns bytes in the HTTP response body.
    Synchronous,
    /// Provider returns a job id the caller polls.
    PollableJob,
    /// Provider accepts a webhook callback URL.
    Webhook,
}

/// Declared provider capability surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoCapabilitySurface {
    /// Supported video verbs.
    pub supported_verbs: BTreeSet<VideoVerb>,
    /// Supported output/input resolutions.
    pub supported_resolutions: BTreeSet<VideoResolution>,
    /// Supported frames-per-second values.
    pub supported_fps: BTreeSet<u8>,
    /// Supported aspect ratios.
    pub supported_aspect_ratios: BTreeSet<AspectRatio>,
    /// Maximum accepted duration in seconds.
    pub max_duration_seconds: u32,
    /// Whether image-to-video conditioning is supported.
    pub supports_image_to_video: bool,
    /// Whether a negative prompt is supported.
    pub supports_negative_prompt: bool,
    /// Whether a deterministic seed is supported.
    pub supports_seed: bool,
    /// Optional model-version pin.
    pub model_version_pinned: Option<ModelVersionPin>,
    /// The provider's async job model.
    pub async_job_model: AsyncJobModel,
}

impl VideoCapabilitySurface {
    /// Validate whether this surface can accept a generate request.
    pub fn check_generate_request(
        &self,
        request: &VideoGenerateRequest,
    ) -> Result<(), VideoRefusalReason> {
        if !self.supported_verbs.contains(&VideoVerb::Generate) {
            return Err(VideoRefusalReason::CapabilitySurfaceExceeded {
                requested: "verb=generate".to_string(),
                supported: self.supported_verb_names(),
            });
        }
        if u32::from(request.duration_seconds) > self.max_duration_seconds {
            return Err(VideoRefusalReason::DurationCeilingExceeded {
                requested: u32::from(request.duration_seconds),
                ceiling: self.max_duration_seconds,
            });
        }
        if !self.supported_resolutions.contains(&request.resolution) {
            return Err(VideoRefusalReason::CapabilitySurfaceExceeded {
                requested: format!("resolution={:?}", request.resolution),
                supported: self
                    .supported_resolutions
                    .iter()
                    .map(|r| format!("{r:?}"))
                    .collect(),
            });
        }
        if !self.supported_fps.contains(&request.fps) {
            return Err(VideoRefusalReason::CapabilitySurfaceExceeded {
                requested: format!("fps={}", request.fps),
                supported: self.supported_fps.iter().map(|f| f.to_string()).collect(),
            });
        }
        if request.reference_image.is_some() && !self.supports_image_to_video {
            return Err(VideoRefusalReason::CapabilitySurfaceExceeded {
                requested: "image_to_video".to_string(),
                supported: self.supported_verb_names(),
            });
        }
        if request.seed.is_some() && !self.supports_seed {
            return Err(VideoRefusalReason::CapabilitySurfaceExceeded {
                requested: "seed".to_string(),
                supported: self.supported_verb_names(),
            });
        }
        Ok(())
    }

    /// Validate whether this surface can accept an analyze request.
    pub fn check_analyze_request(
        &self,
        request: &VideoAnalyzeRequest,
    ) -> Result<(), VideoRefusalReason> {
        if !self.supported_verbs.contains(&VideoVerb::Analyze) {
            return Err(VideoRefusalReason::CapabilitySurfaceExceeded {
                requested: "verb=analyze".to_string(),
                supported: self.supported_verb_names(),
            });
        }
        if request.duration_seconds_upper_bound > self.max_duration_seconds {
            return Err(VideoRefusalReason::DurationCeilingExceeded {
                requested: request.duration_seconds_upper_bound,
                ceiling: self.max_duration_seconds,
            });
        }
        Ok(())
    }

    /// Validate whether this surface can accept a describe request.
    pub fn check_describe_request(
        &self,
        request: &VideoDescribeRequest,
    ) -> Result<(), VideoRefusalReason> {
        if !self.supported_verbs.contains(&VideoVerb::Describe) {
            return Err(VideoRefusalReason::CapabilitySurfaceExceeded {
                requested: "verb=describe".to_string(),
                supported: self.supported_verb_names(),
            });
        }
        let _ = request;
        Ok(())
    }

    fn supported_verb_names(&self) -> Vec<String> {
        self.supported_verbs
            .iter()
            .map(|verb| format!("{verb:?}"))
            .collect()
    }
}

/// Minimal media-provider supertrait until §10.0 lands as its own crate.
pub trait MediaProvider: Send + Sync {
    /// Provider id used for registry lookup and receipts.
    fn media_provider_id(&self) -> &VideoProviderId;
}

/// Closed video provider trait.
#[async_trait]
pub trait VideoProvider: MediaProvider + private::Sealed {
    /// Generate a video from a text prompt (optionally image-conditioned).
    async fn generate(
        &self,
        token: &AuthorizedVideoToken,
        request: VideoGenerateRequest,
    ) -> Result<VideoArtifact, VideoError>;

    /// Describe per-frame at an operator-declared sample rate; optionally
    /// emit an aggregated scene summary.
    async fn describe(
        &self,
        token: &AuthorizedVideoToken,
        request: VideoDescribeRequest,
    ) -> Result<VideoDescribeOutput, VideoError>;

    /// Analyze: action recognition, scene segmentation, OCR, or transcript
    /// extraction.
    async fn analyze(
        &self,
        token: &AuthorizedVideoToken,
        request: VideoAnalyzeRequest,
    ) -> Result<VideoAnalyzeOutput, VideoError>;

    /// Compute the pre-admission cost envelope for a request.
    fn cost_envelope(&self, verb: VideoVerb, request: VideoRequestRef<'_>) -> VideoCostEnvelope;

    /// Return the provider's declared capability surface.
    fn capability_surface(&self) -> VideoCapabilitySurface;
}

mod private {
    pub trait Sealed {}
}

/// Receipt-event vocabulary for video generate/describe/analyze.
pub mod receipt_events {
    /// Plan-native event name for generation success.
    pub const PLAN_VIDEO_GENERATED_V1: &str = "video.generated.v1";
    /// Plan-native event name for describe success.
    pub const PLAN_VIDEO_DESCRIBED_V1: &str = "video.described.v1";
    /// Plan-native event name for analyze success.
    pub const PLAN_VIDEO_ANALYZED_V1: &str = "video.analyzed.v1";
    /// Plan-native event name for catch-all refusal.
    pub const PLAN_VIDEO_REFUSED_V1: &str = "video.refused.v1";

    /// Receipt-grammar-safe generation success verb.
    pub const VIDEO_GENERATED_SUCCEEDED_V1: &str = "video.generated.succeeded.v1";
    /// Receipt-grammar-safe describe success verb.
    pub const VIDEO_DESCRIBED_SUCCEEDED_V1: &str = "video.described.succeeded.v1";
    /// Receipt-grammar-safe analyze success verb.
    pub const VIDEO_ANALYZED_SUCCEEDED_V1: &str = "video.analyzed.succeeded.v1";
    /// Receipt-grammar-safe operation refusal verb.
    pub const VIDEO_OPERATION_REFUSED_V1: &str = "video.operation.refused.v1";
    /// Receipt-grammar-safe duration-ceiling refusal verb.
    pub const VIDEO_DURATION_CEILING_EXCEEDED_V1: &str = "video.duration_ceiling.exceeded.v1";
    /// Receipt-grammar-safe content-class refusal verb.
    pub const VIDEO_CONTENT_CLASS_REFUSED_V1: &str = "video.content_class.refused.v1";
    /// Receipt-grammar-safe tenant opt-in refusal verb.
    pub const VIDEO_TENANT_OPT_IN_REQUIRED_V1: &str = "video.tenant_opt_in.required.v1";
    /// Receipt-grammar-safe frame-sampler failure verb.
    pub const VIDEO_FRAME_SAMPLER_FAILED_V1: &str = "video.frame_sampler.failed.v1";
    /// Receipt-grammar-safe cost-reconciliation success verb.
    pub const VIDEO_COST_RECONCILED_SUCCEEDED_V1: &str = "video.cost_reconciled.succeeded.v1";
    /// Receipt-grammar-safe retention-scheduled success verb.
    pub const VIDEO_RETENTION_SCHEDULED_SUCCEEDED_V1: &str =
        "video.retention_scheduled.succeeded.v1";
    /// Receipt-grammar-safe injection-defense block verb.
    pub const VIDEO_INJECTION_DEFENSE_BLOCKED_V1: &str = "video.injection_defense.blocked.v1";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt_events::*;

    #[test]
    fn generate_cost_applies_resolution_and_fps_multipliers() {
        let envelope = VideoCostEnvelope::Generate {
            duration_seconds: 10,
            per_second_micro_usd: 100_000,
            resolution_multiplier_bps: 18_000,
            fps_multiplier_bps: 10_000,
            floor_micro_usd: 5_000,
        };
        // 10 * 100_000 = 1_000_000; *1.8 = 1_800_000; *1.0 = 1_800_000; +5_000.
        assert_eq!(envelope.total_micro_usd(), 1_805_000);
    }

    #[test]
    fn describe_cost_sums_per_frame_and_overheads() {
        let envelope = VideoCostEnvelope::Describe {
            sampled_frame_count: 10,
            per_frame_describe_micro_usd: 50,
            scene_summary_micro_usd: 200,
            decode_overhead_micro_usd: 30,
            floor_micro_usd: 10,
        };
        assert_eq!(envelope.total_micro_usd(), 10 * 50 + 200 + 30 + 10);
    }

    #[test]
    fn analyze_cost_includes_transcript_delegation() {
        let envelope = VideoCostEnvelope::Analyze {
            duration_seconds: 30,
            per_second_analyze_micro_usd: 200,
            objective_overhead_micro_usd: 1_000,
            transcript_delegate_micro_usd: 500,
            floor_micro_usd: 100,
        };
        assert_eq!(envelope.total_micro_usd(), 30 * 200 + 1_000 + 500 + 100);
    }

    #[test]
    fn content_class_ceiling_uses_closed_ordering() {
        assert!(!ContentClass::Sensitive.exceeds_ceiling(ContentClass::Sensitive));
        assert!(ContentClass::Adult.exceeds_ceiling(ContentClass::Sensitive));
        assert!(ContentClass::Disallowed.exceeds_ceiling(ContentClass::Hateful));
    }

    #[test]
    fn sample_rate_rejects_non_positive_values() {
        assert!(SampleRateFps::new(0.0).is_err());
        assert!(SampleRateFps::new(-1.0).is_err());
        assert!(SampleRateFps::new(f32::NAN).is_err());
        assert!(SampleRateFps::new(1.0).is_ok());
    }

    #[test]
    fn capability_surface_rejects_unsupported_resolution_before_provider_call() {
        let surface = VideoCapabilitySurface {
            supported_verbs: BTreeSet::from([VideoVerb::Generate]),
            supported_resolutions: BTreeSet::from([VideoResolution::R720p]),
            supported_fps: BTreeSet::from([24]),
            supported_aspect_ratios: BTreeSet::from([AspectRatio::Wide16x9]),
            max_duration_seconds: 30,
            supports_image_to_video: false,
            supports_negative_prompt: false,
            supports_seed: false,
            model_version_pinned: None,
            async_job_model: AsyncJobModel::PollableJob,
        };
        let request = VideoGenerateRequest {
            provider_id: VideoProviderId::new("mock"),
            model_id: VideoModelId::new("mock-generate"),
            prompt: "a calm lake at dawn".to_string(),
            negative_prompt: None,
            reference_image: None,
            duration_seconds: 5,
            resolution: VideoResolution::R1080p,
            fps: 24,
            aspect_ratio: AspectRatio::Wide16x9,
            style: None,
            seed: None,
            mission_id: MissionId::new("mission"),
            requested_at: 1,
        };

        assert!(matches!(
            surface.check_generate_request(&request),
            Err(VideoRefusalReason::CapabilitySurfaceExceeded { .. })
        ));
    }

    #[test]
    fn capability_surface_rejects_duration_over_ceiling() {
        let surface = VideoCapabilitySurface {
            supported_verbs: BTreeSet::from([VideoVerb::Generate]),
            supported_resolutions: BTreeSet::from([VideoResolution::R720p]),
            supported_fps: BTreeSet::from([24]),
            supported_aspect_ratios: BTreeSet::from([AspectRatio::Wide16x9]),
            max_duration_seconds: 5,
            supports_image_to_video: false,
            supports_negative_prompt: false,
            supports_seed: false,
            model_version_pinned: None,
            async_job_model: AsyncJobModel::PollableJob,
        };
        let request = VideoGenerateRequest {
            provider_id: VideoProviderId::new("mock"),
            model_id: VideoModelId::new("mock-generate"),
            prompt: "a calm lake at dawn".to_string(),
            negative_prompt: None,
            reference_image: None,
            duration_seconds: 30,
            resolution: VideoResolution::R720p,
            fps: 24,
            aspect_ratio: AspectRatio::Wide16x9,
            style: None,
            seed: None,
            mission_id: MissionId::new("mission"),
            requested_at: 1,
        };

        assert!(matches!(
            surface.check_generate_request(&request),
            Err(VideoRefusalReason::DurationCeilingExceeded { .. })
        ));
    }

    #[test]
    fn normalized_receipt_events_match_current_receipt_grammar() {
        for verb in [
            VIDEO_GENERATED_SUCCEEDED_V1,
            VIDEO_DESCRIBED_SUCCEEDED_V1,
            VIDEO_ANALYZED_SUCCEEDED_V1,
            VIDEO_OPERATION_REFUSED_V1,
            VIDEO_DURATION_CEILING_EXCEEDED_V1,
            VIDEO_CONTENT_CLASS_REFUSED_V1,
            VIDEO_TENANT_OPT_IN_REQUIRED_V1,
            VIDEO_FRAME_SAMPLER_FAILED_V1,
            VIDEO_COST_RECONCILED_SUCCEEDED_V1,
            VIDEO_RETENTION_SCHEDULED_SUCCEEDED_V1,
            VIDEO_INJECTION_DEFENSE_BLOCKED_V1,
        ] {
            ardur_receipt::VerbObject::new(verb).expect("video receipt verb is grammar-safe");
        }
    }

    #[test]
    fn plan_native_event_names_are_kept_for_traceability() {
        assert_eq!(PLAN_VIDEO_GENERATED_V1, "video.generated.v1");
        assert_eq!(PLAN_VIDEO_DESCRIBED_V1, "video.described.v1");
        assert_eq!(PLAN_VIDEO_ANALYZED_V1, "video.analyzed.v1");
        assert_eq!(PLAN_VIDEO_REFUSED_V1, "video.refused.v1");
    }
}
