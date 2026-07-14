//! Gemini video-input adapter: the one concrete [`VideoProvider`] this crate
//! ships in Phase 1.
//!
//! `generate` needs an async-job-polling provider (Sora/Runway/Veo/Pika) and
//! `describe` needs the not-yet-landed FFmpeg frame sampler (§10.2
//! architecture, `media-decode`); both are left as
//! [`VideoError::Refused`]-returning stubs here, mirroring how
//! `media-audio`'s Whisper adapter stubs streaming and diarization. `analyze`
//! is fully wired: it sends whole-video bytes to Gemini's multimodal
//! `generateContent` endpoint (Gemini accepts video input directly — no
//! frame sampling required) and parses a structured JSON result per
//! objective.

use std::sync::Arc;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    ActionSegment, AspectRatio, AsyncJobModel, AuthorizedVideoToken, ContentClass, MediaProvider,
    MissionId, ModelVersionPin, OcrTemporalSegment, ReceiptHash, SceneSegment, UnixTsMillis,
    VideoAnalyzeObjective, VideoAnalyzeOutput, VideoAnalyzeRequest, VideoAnalyzeStructured,
    VideoArtifact, VideoCapabilitySurface, VideoCostEnvelope, VideoDescribeOutput,
    VideoDescribeRequest, VideoError, VideoFormat, VideoGenerateRequest, VideoInput, VideoModelId,
    VideoProvider, VideoProviderId, VideoRefusalReason, VideoRequestRef, VideoResolution,
    VideoScope, VideoVerb,
};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/";
const DEFAULT_MODEL: &str = "gemini-2.5-pro";
const DEFAULT_PROVIDER_ID: &str = "gemini-video-in";
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_DURATION_SECONDS: u32 = 10 * 60;
const MAX_INLINE_VIDEO_BYTES: usize = 20 * 1024 * 1024;
const MAX_UPSTREAM_ERROR_BYTES: usize = 512;

/// Configuration for [`GeminiVideoAnalyzeProvider`].
#[derive(Clone)]
pub struct GeminiVideoConfig {
    api_key: String,
    base_url: Url,
    model: VideoModelId,
    provider_id: VideoProviderId,
    timeout: Duration,
}

impl std::fmt::Debug for GeminiVideoConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiVideoConfig")
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url.as_str())
            .field("model", &self.model)
            .field("provider_id", &self.provider_id)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl GeminiVideoConfig {
    /// Build a config using the default Gemini `v1beta` base URL and model.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: Url::parse(DEFAULT_BASE_URL).expect("default Gemini URL is valid"),
            model: VideoModelId::new(DEFAULT_MODEL),
            provider_id: VideoProviderId::new(DEFAULT_PROVIDER_ID),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }

    /// Override the API base URL. HTTPS is required except for loopback
    /// HTTP, which is allowed for deterministic integration tests.
    pub fn with_base_url(mut self, base_url: impl AsRef<str>) -> Result<Self, VideoError> {
        self.base_url = validate_base_url(base_url.as_ref())?;
        Ok(self)
    }

    /// Override the default Gemini model id.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = VideoModelId::new(model);
        self
    }

    /// Override the provider id exposed to policy and receipts.
    #[must_use]
    pub fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = VideoProviderId::new(provider_id);
        self
    }

    /// Override the HTTP request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Gemini video-input-backed analyze (and, later, describe) provider.
#[derive(Clone)]
pub struct GeminiVideoAnalyzeProvider {
    config: GeminiVideoConfig,
    client: reqwest::Client,
}

impl std::fmt::Debug for GeminiVideoAnalyzeProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiVideoAnalyzeProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl GeminiVideoAnalyzeProvider {
    /// Construct a provider, validating that the API key and HTTP client
    /// config are usable before the provider is registered.
    pub fn new(config: GeminiVideoConfig) -> Result<Self, VideoError> {
        if config.api_key.trim().is_empty() {
            return Err(VideoError::InvalidRequest(
                "Gemini API key must be non-empty".to_string(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| VideoError::Provider(format!("building Gemini HTTP client: {e}")))?;
        Ok(Self { config, client })
    }

    /// Build from `GEMINI_API_KEY`, returning `None` when no key is
    /// configured so server boot can degrade gracefully.
    pub fn from_env() -> Result<Option<Self>, VideoError> {
        let key = std::env::var("GEMINI_API_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let Some(key) = key else {
            return Ok(None);
        };
        let mut config = GeminiVideoConfig::new(key);
        if let Ok(base_url) = std::env::var("GEMINI_BASE_URL") {
            if !base_url.trim().is_empty() {
                config = config.with_base_url(base_url)?;
            }
        }
        if let Ok(model) = std::env::var("GEMINI_VIDEO_MODEL") {
            if !model.trim().is_empty() {
                config = config.with_model(model);
            }
        }
        Self::new(config).map(Some)
    }

    fn endpoint(&self) -> Result<Url, VideoError> {
        self.config
            .base_url
            .join(&format!(
                "models/{}:generateContent",
                self.config.model.as_str()
            ))
            .map_err(|e| VideoError::InvalidRequest(format!("invalid Gemini endpoint: {e}")))
    }
}

impl MediaProvider for GeminiVideoAnalyzeProvider {
    fn media_provider_id(&self) -> &VideoProviderId {
        &self.config.provider_id
    }
}

impl crate::private::Sealed for GeminiVideoAnalyzeProvider {}

fn objective_instruction(objective: &VideoAnalyzeObjective) -> &'static str {
    match objective {
        VideoAnalyzeObjective::ActionRecognition => {
            "Identify distinct human/animal activities in this video. Respond with strict JSON: \
             {\"segments\":[{\"start_seconds\":number,\"end_seconds\":number,\"action_label\":string,\"confidence\":number}]}. \
             Nothing else."
        }
        VideoAnalyzeObjective::SceneSegmentation => {
            "Partition this video into distinct scenes/shots. Respond with strict JSON: \
             {\"scenes\":[{\"start_seconds\":number,\"end_seconds\":number,\"description\":string}]}. \
             Nothing else."
        }
        VideoAnalyzeObjective::OnScreenTextOcr => {
            "Transcribe any on-screen text visible in this video, with the time range each string \
             is visible. Respond with strict JSON: \
             {\"segments\":[{\"start_seconds\":number,\"end_seconds\":number,\"text\":string}]}. \
             Nothing else."
        }
        VideoAnalyzeObjective::ExtractTranscript { .. } => "",
    }
}

#[derive(Debug, Deserialize)]
struct ActionRecognitionJson {
    #[serde(default)]
    segments: Vec<ActionSegmentJson>,
}

#[derive(Debug, Deserialize)]
struct ActionSegmentJson {
    start_seconds: f32,
    end_seconds: f32,
    action_label: String,
    #[serde(default)]
    confidence: f32,
}

#[derive(Debug, Deserialize)]
struct SceneSegmentationJson {
    #[serde(default)]
    scenes: Vec<SceneSegmentJson>,
}

#[derive(Debug, Deserialize)]
struct SceneSegmentJson {
    start_seconds: f32,
    end_seconds: f32,
    description: String,
}

#[derive(Debug, Deserialize)]
struct OcrJson {
    #[serde(default)]
    segments: Vec<OcrSegmentJson>,
}

#[derive(Debug, Deserialize)]
struct OcrSegmentJson {
    start_seconds: f32,
    end_seconds: f32,
    text: String,
}

fn parse_structured(
    objective: &VideoAnalyzeObjective,
    text: &str,
) -> Result<VideoAnalyzeStructured, VideoError> {
    let cleaned = strip_code_fence(text);
    match objective {
        VideoAnalyzeObjective::ActionRecognition => {
            let parsed: ActionRecognitionJson = serde_json::from_str(cleaned)
                .map_err(|e| VideoError::Provider(format!("parsing Gemini analyze JSON: {e}")))?;
            Ok(VideoAnalyzeStructured::ActionRecognition {
                segments: parsed
                    .segments
                    .into_iter()
                    .map(|s| ActionSegment {
                        start_seconds: s.start_seconds,
                        end_seconds: s.end_seconds,
                        action_label: s.action_label,
                        confidence: s.confidence,
                    })
                    .collect(),
            })
        }
        VideoAnalyzeObjective::SceneSegmentation => {
            let parsed: SceneSegmentationJson = serde_json::from_str(cleaned)
                .map_err(|e| VideoError::Provider(format!("parsing Gemini analyze JSON: {e}")))?;
            Ok(VideoAnalyzeStructured::SceneSegmentation {
                scenes: parsed
                    .scenes
                    .into_iter()
                    .map(|s| SceneSegment {
                        start_seconds: s.start_seconds,
                        end_seconds: s.end_seconds,
                        representative_frame: None,
                        description: s.description,
                    })
                    .collect(),
            })
        }
        VideoAnalyzeObjective::OnScreenTextOcr => {
            let parsed: OcrJson = serde_json::from_str(cleaned)
                .map_err(|e| VideoError::Provider(format!("parsing Gemini analyze JSON: {e}")))?;
            Ok(VideoAnalyzeStructured::OnScreenTextOcr {
                segments: parsed
                    .segments
                    .into_iter()
                    .map(|s| OcrTemporalSegment {
                        start_seconds: s.start_seconds,
                        end_seconds: s.end_seconds,
                        text: s.text,
                    })
                    .collect(),
            })
        }
        VideoAnalyzeObjective::ExtractTranscript { .. } => {
            Err(VideoRefusalReason::TranscriptDelegationFailed {
                underlying: "§10.3 transcript delegation is a forward-ref; this adapter cannot \
                             extract transcripts"
                    .to_string(),
            }
            .into())
        }
    }
}

fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|rest| rest.strip_suffix("```").unwrap_or(rest).trim())
        .unwrap_or(trimmed)
}

/// Extract every human-readable string a caller could see from a structured
/// analyze result, for the injection-defense scan.
#[must_use]
pub fn structured_text_for_scan(structured: &VideoAnalyzeStructured) -> String {
    match structured {
        VideoAnalyzeStructured::ActionRecognition { segments } => segments
            .iter()
            .map(|s| s.action_label.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        VideoAnalyzeStructured::SceneSegmentation { scenes } => scenes
            .iter()
            .map(|s| s.description.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        VideoAnalyzeStructured::OnScreenTextOcr { segments } => segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        VideoAnalyzeStructured::ExtractTranscript {
            transcript_segments,
            ..
        } => transcript_segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[async_trait::async_trait]
impl VideoProvider for GeminiVideoAnalyzeProvider {
    async fn generate(
        &self,
        _token: &AuthorizedVideoToken,
        _request: VideoGenerateRequest,
    ) -> Result<VideoArtifact, VideoError> {
        Err(VideoError::InvalidRequest(
            "GeminiVideoAnalyzeProvider does not implement generate; use a Sora/Runway/Veo/Pika \
             adapter"
                .to_string(),
        ))
    }

    async fn describe(
        &self,
        _token: &AuthorizedVideoToken,
        _request: VideoDescribeRequest,
    ) -> Result<VideoDescribeOutput, VideoError> {
        Err(VideoError::InvalidRequest(
            "GeminiVideoAnalyzeProvider does not implement describe; per-frame describe needs \
             the media-decode frame sampler"
                .to_string(),
        ))
    }

    async fn analyze(
        &self,
        token: &AuthorizedVideoToken,
        request: VideoAnalyzeRequest,
    ) -> Result<VideoAnalyzeOutput, VideoError> {
        validate_authorized_scope(token, &request)?;
        self.capability_surface().check_analyze_request(&request)?;

        let bytes = load_video_bytes(&request.input)?;
        if bytes.is_empty() {
            return Err(VideoError::InvalidRequest(
                "video bytes must be non-empty".to_string(),
            ));
        }
        if bytes.len() > MAX_INLINE_VIDEO_BYTES {
            return Err(VideoError::InvalidRequest(format!(
                "video decodes to {} bytes, exceeding maximum {MAX_INLINE_VIDEO_BYTES}",
                bytes.len()
            )));
        }
        let instruction = objective_instruction(&request.objective);
        if instruction.is_empty() {
            // ExtractTranscript: no Gemini call needed, refuse up front.
            return Err(VideoRefusalReason::TranscriptDelegationFailed {
                underlying: "§10.3 transcript delegation is a forward-ref".to_string(),
            }
            .into());
        }

        let body = serde_json::json!({
            "contents": [{
                "parts": [
                    { "inline_data": { "mime_type": request.input.format().mime_type(), "data": BASE64_STANDARD.encode(&bytes) } },
                    { "text": instruction },
                ],
            }],
            "generationConfig": { "response_mime_type": "application/json" },
        });

        let response = self
            .client
            .post(self.endpoint()?)
            .query(&[("key", self.config.api_key.as_str())])
            .json(&body)
            .send()
            .await
            .map_err(|e| VideoError::Provider(format!("calling Gemini API: {e}")))?;
        let status = response.status();
        let response_body = response
            .bytes()
            .await
            .map_err(|e| VideoError::Provider(format!("reading Gemini response: {e}")))?;
        if !status.is_success() {
            return Err(VideoError::Provider(format!(
                "Gemini API returned {status}: {}",
                sanitize_upstream_error(&response_body)
            )));
        }
        let parsed: GeminiGenerateContentResponse = serde_json::from_slice(&response_body)
            .map_err(|e| VideoError::Provider(format!("parsing Gemini response: {e}")))?;
        let text = parsed
            .candidates
            .first()
            .and_then(|c| c.content.parts.first())
            .map(|p| p.text.as_str())
            .ok_or_else(|| VideoError::Provider("Gemini returned no candidates".to_string()))?;

        let structured = parse_structured(&request.objective, text)?;
        let video_hash = digest_hex(&bytes);
        let receipt_hash = ReceiptHash::new(digest_hex(format!(
            "analyze;video={video_hash};objective={:?}",
            request.objective
        )));
        let cost_envelope =
            self.cost_envelope(VideoVerb::Analyze, VideoRequestRef::Analyze(&request));

        Ok(VideoAnalyzeOutput {
            provider_id: self.config.provider_id.clone(),
            model_id: request.model_id,
            objective: request.objective,
            structured,
            content_class: ContentClass::Safe,
            cost_envelope,
            created_at: request.requested_at,
            receipt_hash,
        })
    }

    fn cost_envelope(&self, verb: VideoVerb, request: VideoRequestRef<'_>) -> VideoCostEnvelope {
        match (verb, request) {
            (VideoVerb::Analyze, VideoRequestRef::Analyze(req)) => VideoCostEnvelope::Analyze {
                duration_seconds: req.duration_seconds_upper_bound,
                per_second_analyze_micro_usd: 50,
                objective_overhead_micro_usd: 500,
                transcript_delegate_micro_usd: 0,
                floor_micro_usd: 1_000,
            },
            (VideoVerb::Generate, VideoRequestRef::Generate(req)) => VideoCostEnvelope::Generate {
                duration_seconds: u32::from(req.duration_seconds),
                per_second_micro_usd: 0,
                resolution_multiplier_bps: 10_000,
                fps_multiplier_bps: 10_000,
                floor_micro_usd: 0,
            },
            (VideoVerb::Describe, VideoRequestRef::Describe(_)) => VideoCostEnvelope::Describe {
                sampled_frame_count: 0,
                per_frame_describe_micro_usd: 0,
                scene_summary_micro_usd: 0,
                decode_overhead_micro_usd: 0,
                floor_micro_usd: 0,
            },
            _ => VideoCostEnvelope::Analyze {
                duration_seconds: 0,
                per_second_analyze_micro_usd: 0,
                objective_overhead_micro_usd: 0,
                transcript_delegate_micro_usd: 0,
                floor_micro_usd: 0,
            },
        }
    }

    fn capability_surface(&self) -> VideoCapabilitySurface {
        use std::collections::BTreeSet;
        VideoCapabilitySurface {
            supported_verbs: BTreeSet::from([VideoVerb::Analyze]),
            supported_resolutions: BTreeSet::from([
                VideoResolution::R480p,
                VideoResolution::R720p,
                VideoResolution::R1080p,
                VideoResolution::R4k,
            ]),
            supported_fps: BTreeSet::from([24, 30, 60]),
            supported_aspect_ratios: BTreeSet::from([
                AspectRatio::Wide16x9,
                AspectRatio::Tall9x16,
                AspectRatio::Square1x1,
                AspectRatio::Standard4x3,
            ]),
            max_duration_seconds: MAX_DURATION_SECONDS,
            supports_image_to_video: false,
            supports_negative_prompt: false,
            supports_seed: false,
            model_version_pinned: Some(ModelVersionPin::new(DEFAULT_MODEL)),
            async_job_model: AsyncJobModel::Synchronous,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GeminiGenerateContentResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: GeminiContent,
}

#[derive(Debug, Deserialize)]
struct GeminiContent {
    #[serde(default)]
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Deserialize)]
struct GeminiPart {
    #[serde(default)]
    text: String,
}

fn validate_authorized_scope(
    token: &AuthorizedVideoToken,
    request: &VideoAnalyzeRequest,
) -> Result<(), VideoError> {
    if token.cap_token.0.trim().is_empty() {
        return Err(VideoError::InvalidRequest(
            "cap-token must be non-empty".to_string(),
        ));
    }
    if token.scope.verb != VideoVerb::Analyze || token.scope.provider_id != request.provider_id {
        return Err(VideoRefusalReason::CapTokenInsufficient {
            required_scope: VideoScope {
                verb: VideoVerb::Analyze,
                provider_id: request.provider_id.clone(),
                content_class_ceiling: ContentClass::Safe,
                duration_seconds_ceiling: request.duration_seconds_upper_bound,
                sample_rate_fps_ceiling: None,
            },
        }
        .into());
    }
    if request.duration_seconds_upper_bound > token.scope.duration_seconds_ceiling {
        return Err(VideoRefusalReason::DurationCeilingExceeded {
            requested: request.duration_seconds_upper_bound,
            ceiling: token.scope.duration_seconds_ceiling,
        }
        .into());
    }
    Ok(())
}

fn load_video_bytes(input: &VideoInput) -> Result<Vec<u8>, VideoError> {
    match input {
        VideoInput::InlineBytes { bytes, .. } => Ok(bytes.clone()),
        VideoInput::WorkspacePath { path, .. } => read_workspace_video(path),
        VideoInput::Artifact { .. } => Err(VideoError::InvalidRequest(
            "artifact video input is not supported by GeminiVideoAnalyzeProvider".to_string(),
        )),
    }
}

fn read_workspace_video(path: &std::path::Path) -> Result<Vec<u8>, VideoError> {
    use std::path::Component;
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(VideoRefusalReason::WorkspaceScopeRefused {
            path: path.to_path_buf(),
        }
        .into());
    }
    std::fs::read(std::path::PathBuf::from(path))
        .map_err(|e| VideoError::Provider(format!("reading workspace video: {e}")))
}

fn validate_base_url(raw: &str) -> Result<Url, VideoError> {
    let mut parsed = Url::parse(raw)
        .map_err(|e| VideoError::InvalidRequest(format!("invalid Gemini base URL: {e}")))?;
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(VideoError::InvalidRequest(
            "Gemini base URL must not contain query or fragment components".to_string(),
        ));
    }
    match parsed.scheme() {
        "https" => normalize_directory_base_url(&mut parsed),
        "http" if is_loopback_url(&parsed) => normalize_directory_base_url(&mut parsed),
        scheme => Err(VideoError::InvalidRequest(format!(
            "Gemini base URL must be HTTPS or loopback HTTP, got `{scheme}`"
        ))),
    }
}

fn normalize_directory_base_url(url: &mut Url) -> Result<Url, VideoError> {
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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Tool wrapper that exposes an analyze provider as `video.analyze`.
///
/// Provider-returned text (action labels, scene descriptions, on-screen OCR)
/// is untrusted content sourced from an external video; before it is handed
/// back to the caller, it is scanned by `ardur-injection-defense`'s
/// `PatternBasedFilter` and blocked/sanitized like any other tool output.
pub struct VideoAnalyzeTool {
    provider: Arc<dyn VideoProvider>,
    schema: ardur_tool_registry::ToolSchema,
    capabilities: Vec<ardur_tool_registry::Capability>,
    default_model: VideoModelId,
    injection_filters: ardur_injection_defense::FilterRegistry,
}

impl VideoAnalyzeTool {
    /// Stable tool id registered in [`ardur_tool_registry::ToolRegistry`].
    pub const ID: &'static str = "video.analyze";

    /// Wrap a concrete provider as the `video.analyze` tool.
    #[must_use]
    pub fn new(provider: impl VideoProvider + 'static) -> Self {
        Self::from_arc(Arc::new(provider))
    }

    /// Wrap an already-shared provider as the `video.analyze` tool.
    #[must_use]
    pub fn from_arc(provider: Arc<dyn VideoProvider>) -> Self {
        let injection_filters = ardur_injection_defense::FilterRegistry::new();
        injection_filters.register(Arc::new(ardur_injection_defense::PatternBasedFilter::new()));
        Self {
            provider,
            schema: video_analyze_schema(),
            capabilities: vec![
                ardur_tool_registry::Capability::NetworkOut,
                ardur_tool_registry::Capability::Custom(Self::ID.to_string()),
            ],
            default_model: VideoModelId::new(DEFAULT_MODEL),
            injection_filters,
        }
    }

    async fn scan_for_injection(
        &self,
        text: &str,
    ) -> Result<String, ardur_tool_registry::ToolError> {
        if text.trim().is_empty() {
            return Ok(text.to_string());
        }
        let content = ardur_injection_defense::ScannableContent::ToolOutput {
            tool_id: ardur_tool_registry::ToolId::new(Self::ID),
            output: serde_json::json!({ "text": text }),
        };
        let result = self
            .injection_filters
            .scan_all(&content)
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;
        match result.verdict {
            ardur_injection_defense::Verdict::Allow => Ok(text.to_string()),
            ardur_injection_defense::Verdict::AllowWithSanitization { sanitized } => Ok(sanitized),
            ardur_injection_defense::Verdict::Block { reason } => {
                Err(ardur_tool_registry::ToolError::ExecutionFailed(
                    VideoError::from(VideoRefusalReason::InjectionDefenseBlocked { reason })
                        .to_string(),
                ))
            }
        }
    }
}

#[async_trait::async_trait]
impl ardur_tool_registry::Tool for VideoAnalyzeTool {
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
                reason: "missing cap-token for video.analyze".to_string(),
            });
        }
        let parsed = VideoAnalyzeArgs::parse(args)?;
        let request = parsed.to_request(
            self.provider.media_provider_id().clone(),
            self.default_model.clone(),
        )?;
        let token = AuthorizedVideoToken {
            cap_token: ctx.cap_token.clone(),
            scope: VideoScope {
                verb: VideoVerb::Analyze,
                provider_id: self.provider.media_provider_id().clone(),
                content_class_ceiling: ContentClass::Safe,
                duration_seconds_ceiling: MAX_DURATION_SECONDS,
                sample_rate_fps_ceiling: None,
            },
        };
        let envelope = self
            .provider
            .cost_envelope(VideoVerb::Analyze, VideoRequestRef::Analyze(&request));
        let output = self
            .provider
            .analyze(&token, request)
            .await
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e.to_string()))?;

        let raw_text = structured_text_for_scan(&output.structured);
        let scanned_text = self.scan_for_injection(&raw_text).await?;

        let cents = envelope.total_micro_usd().div_ceil(10_000);
        Ok(ardur_tool_registry::ToolOutput {
            content: serde_json::json!({
                "objective": format!("{:?}", output.objective),
                "summary": scanned_text,
                "provider_id": output.provider_id.as_str(),
                "model_id": output.model_id.as_str(),
                "receipt_hash": output.receipt_hash.as_str(),
            }),
            cost: ardur_runtime::CostTuple {
                cents,
                wall_ms: 0,
                ..Default::default()
            },
            receipt_data: serde_json::json!({
                "operation": Self::ID,
                "provider_id": output.provider_id.as_str(),
                "model_id": output.model_id.as_str(),
                "receipt_hash": output.receipt_hash.as_str(),
                "cost_envelope_micro_usd": envelope.total_micro_usd(),
            }),
        })
    }

    fn required_capabilities(&self) -> &[ardur_tool_registry::Capability] {
        &self.capabilities
    }
}

#[derive(Debug)]
struct VideoAnalyzeArgs {
    video_base64: String,
    format: VideoFormat,
    duration_seconds_upper_bound: u32,
    objective: VideoAnalyzeObjective,
    model_id: Option<VideoModelId>,
    mission_id: MissionId,
}

impl VideoAnalyzeArgs {
    fn parse(value: serde_json::Value) -> Result<Self, ardur_tool_registry::ToolError> {
        let object = value.as_object().ok_or_else(|| {
            ardur_tool_registry::ToolError::InvalidArgs(
                "arguments must be a JSON object".to_string(),
            )
        })?;
        let video_base64 = object
            .get("video_base64")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                ardur_tool_registry::ToolError::InvalidArgs(
                    "video_base64 is required and must be non-empty".to_string(),
                )
            })?
            .to_string();
        let format = object
            .get("format")
            .and_then(serde_json::Value::as_str)
            .map(parse_video_format)
            .transpose()?
            .unwrap_or(VideoFormat::Mp4H264);
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
        let objective = object
            .get("objective")
            .and_then(serde_json::Value::as_str)
            .map(parse_objective)
            .transpose()?
            .unwrap_or(VideoAnalyzeObjective::SceneSegmentation);
        let model_id = object
            .get("model_id")
            .and_then(serde_json::Value::as_str)
            .map(VideoModelId::new);
        let mission_id = object
            .get("mission_id")
            .and_then(serde_json::Value::as_str)
            .map(MissionId::new)
            .unwrap_or_else(|| MissionId::new("video.analyze"));
        Ok(Self {
            video_base64,
            format,
            duration_seconds_upper_bound,
            objective,
            model_id,
            mission_id,
        })
    }

    fn to_request(
        &self,
        provider_id: VideoProviderId,
        default_model: VideoModelId,
    ) -> Result<VideoAnalyzeRequest, ardur_tool_registry::ToolError> {
        let bytes = BASE64_STANDARD
            .decode(self.video_base64.as_bytes())
            .map_err(|e| {
                ardur_tool_registry::ToolError::InvalidArgs(format!("invalid video_base64: {e}"))
            })?;
        if bytes.len() > MAX_INLINE_VIDEO_BYTES {
            return Err(ardur_tool_registry::ToolError::InvalidArgs(format!(
                "video_base64 decodes to {} bytes, exceeding maximum {MAX_INLINE_VIDEO_BYTES}",
                bytes.len()
            )));
        }
        Ok(VideoAnalyzeRequest {
            provider_id,
            model_id: self.model_id.clone().unwrap_or(default_model),
            input: VideoInput::InlineBytes {
                bytes,
                format: self.format,
            },
            objective: self.objective.clone(),
            duration_seconds_upper_bound: self.duration_seconds_upper_bound,
            mission_id: self.mission_id.clone(),
            requested_at: unix_millis_now(),
        })
    }
}

fn parse_video_format(raw: &str) -> Result<VideoFormat, ardur_tool_registry::ToolError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "mp4" | "mp4h264" => Ok(VideoFormat::Mp4H264),
        "hevc" | "mp4h265" => Ok(VideoFormat::Mp4H265),
        "webm" => Ok(VideoFormat::WebmVp9),
        "mov" => Ok(VideoFormat::Mov),
        other => Err(ardur_tool_registry::ToolError::InvalidArgs(format!(
            "unsupported video format `{other}`"
        ))),
    }
}

fn parse_objective(raw: &str) -> Result<VideoAnalyzeObjective, ardur_tool_registry::ToolError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "action_recognition" => Ok(VideoAnalyzeObjective::ActionRecognition),
        "scene_segmentation" => Ok(VideoAnalyzeObjective::SceneSegmentation),
        "on_screen_text_ocr" => Ok(VideoAnalyzeObjective::OnScreenTextOcr),
        "extract_transcript" => Ok(VideoAnalyzeObjective::ExtractTranscript {
            diarize: false,
            language_tag: None,
        }),
        other => Err(ardur_tool_registry::ToolError::InvalidArgs(format!(
            "unsupported analyze objective `{other}`"
        ))),
    }
}

fn video_analyze_schema() -> ardur_tool_registry::ToolSchema {
    ardur_tool_registry::ToolSchema {
        description:
            "Analyze a small base64-encoded video clip for actions, scenes, or on-screen text \
             with the configured Gemini video-input provider."
                .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["video_base64", "duration_seconds_upper_bound"],
            "properties": {
                "video_base64": { "type": "string", "description": "Base64 encoded video bytes." },
                "format": { "type": "string", "enum": ["mp4", "hevc", "webm", "mov"], "default": "mp4" },
                "duration_seconds_upper_bound": { "type": "integer", "minimum": 1 },
                "objective": {
                    "type": "string",
                    "enum": ["action_recognition", "scene_segmentation", "on_screen_text_ocr", "extract_transcript"],
                    "default": "scene_segmentation"
                },
                "model_id": { "type": "string" },
                "mission_id": { "type": "string" }
            }
        }),
        output_schema: serde_json::json!({
            "type": "object",
            "required": ["objective", "summary", "receipt_hash"],
            "properties": {
                "objective": { "type": "string" },
                "summary": { "type": "string" },
                "provider_id": { "type": "string" },
                "model_id": { "type": "string" },
                "receipt_hash": { "type": "string" }
            }
        }),
        examples: vec![ardur_tool_registry::ToolExample {
            description: "Segment a short clip into scenes.".to_string(),
            args: serde_json::json!({
                "video_base64": "AAAAIGZ0eXBpc29t",
                "format": "mp4",
                "duration_seconds_upper_bound": 10,
                "objective": "scene_segmentation"
            }),
            output: serde_json::json!({
                "objective": "SceneSegmentation",
                "summary": "a lake at dawn\na person walks into frame",
                "provider_id": DEFAULT_PROVIDER_ID,
                "model_id": DEFAULT_MODEL,
                "receipt_hash": "..."
            }),
        }],
    }
}
