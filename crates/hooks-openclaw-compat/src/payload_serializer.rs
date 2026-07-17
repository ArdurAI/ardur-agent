use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event_name_map::{
    CanonicalHookEventName, OpenClawCodexEventName, OpenClawHookEventNameMap,
    OpenClawNativeEventName,
};

/// OpenClaw hook provider namespace. The §9.6 compatibility surface currently
/// accepts only the codex provider OpenClaw exposes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenClawHookProvider {
    /// Codex-compatible hook provider.
    #[default]
    Codex,
}

impl OpenClawHookProvider {
    /// Stable wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
        }
    }
}

/// Metadata supplied by hook registration/runtime state for codex payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenClawHookMeta {
    /// Provider namespace; currently always codex.
    pub provider: OpenClawHookProvider,
    /// Locally generated relay id preserved for codex payload parity.
    pub relay_id: String,
    /// Optional OpenClaw agent id.
    pub agent_id: Option<String>,
    /// Optional OpenClaw session key.
    pub session_key: Option<String>,
    /// Run identifier for this hook fire.
    pub run_id: String,
    /// Optional model name.
    pub model: Option<String>,
    /// Optional turn id.
    pub turn_id: Option<String>,
    /// Optional transcript path.
    pub transcript_path: Option<PathBuf>,
    /// Optional permission mode copied into the codex payload.
    pub permission_mode: Option<String>,
    /// Optional stop-hook-active flag copied into the codex payload.
    pub stop_hook_active: Option<bool>,
    /// Optional final assistant message copied into before-agent-finalize events.
    pub last_assistant_message: Option<String>,
    /// Optional tool-use id.
    pub tool_use_id: Option<String>,
}

impl OpenClawHookMeta {
    /// Create required OpenClaw metadata for a codex payload.
    #[must_use]
    pub fn new(relay_id: impl Into<String>, run_id: impl Into<String>) -> Self {
        Self {
            provider: OpenClawHookProvider::Codex,
            relay_id: relay_id.into(),
            agent_id: None,
            session_key: None,
            run_id: run_id.into(),
            model: None,
            turn_id: None,
            transcript_path: None,
            permission_mode: None,
            stop_hook_active: None,
            last_assistant_message: None,
            tool_use_id: None,
        }
    }
}

/// Canonical hook-fire payload fields needed to emit codex stdin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CanonicalHookFirePayload {
    /// Ardur canonical event name.
    pub event: CanonicalHookEventName,
    /// Session id copied into codex `sessionId`.
    pub session_id: String,
    /// Optional current working directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Optional tool name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// The original canonical event payload preserved under codex `rawPayload`.
    pub raw_payload: Value,
    /// Receipt/fire timestamp in ISO-8601 form.
    pub received_at: String,
}

impl CanonicalHookFirePayload {
    /// Create a canonical payload for serialization into the codex format.
    #[must_use]
    pub fn new(
        event: CanonicalHookEventName,
        session_id: impl Into<String>,
        raw_payload: Value,
        received_at: impl Into<String>,
    ) -> Self {
        Self {
            event,
            session_id: session_id.into(),
            cwd: None,
            tool_name: None,
            raw_payload,
            received_at: received_at.into(),
        }
    }
}

/// The OpenClaw codex stdin payload shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexStdinPayload {
    /// Provider namespace.
    pub provider: OpenClawHookProvider,
    /// Relay id preserved for compatibility with OpenClaw hook scripts.
    pub relay_id: String,
    /// Codex event name.
    pub event: OpenClawCodexEventName,
    /// PascalCase native hook event name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_event_name: Option<OpenClawNativeEventName>,
    /// Optional OpenClaw agent id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Session id.
    pub session_id: String,
    /// Optional session key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    /// Run id.
    pub run_id: String,
    /// Optional current working directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Optional model name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Optional turn id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Optional transcript path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<PathBuf>,
    /// Optional permission mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Optional stop-hook-active flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_hook_active: Option<bool>,
    /// Optional final assistant message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_assistant_message: Option<String>,
    /// Optional tool name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Optional tool-use id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Original canonical event payload.
    pub raw_payload: Value,
    /// Receipt/fire timestamp in ISO-8601 form.
    pub received_at: String,
}

/// Serializer failure surface.
#[derive(Debug, thiserror::Error)]
pub enum SerializeError {
    /// Required field missing or blank.
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    /// JSON serialization failed.
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Closed trait for serializers that emit OpenClaw/codex stdin payloads.
pub trait OpenClawPayloadSerializer: crate::sealed::Sealed + Send + Sync + 'static {
    /// Serialize canonical hook fields and OpenClaw metadata into codex stdin.
    fn serialize(
        &self,
        canonical_payload: &CanonicalHookFirePayload,
        openclaw_meta: &OpenClawHookMeta,
    ) -> Result<Vec<u8>, SerializeError>;

    /// Build the typed codex payload before JSON encoding.
    fn to_payload(
        &self,
        canonical_payload: &CanonicalHookFirePayload,
        openclaw_meta: &OpenClawHookMeta,
    ) -> Result<CodexStdinPayload, SerializeError>;
}

/// Default serializer for the codex `NativeHookRelayInvocation` shape.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultOpenClawPayloadSerializer;

impl crate::sealed::Sealed for DefaultOpenClawPayloadSerializer {}

impl OpenClawPayloadSerializer for DefaultOpenClawPayloadSerializer {
    fn serialize(
        &self,
        canonical_payload: &CanonicalHookFirePayload,
        openclaw_meta: &OpenClawHookMeta,
    ) -> Result<Vec<u8>, SerializeError> {
        let payload = self.to_payload(canonical_payload, openclaw_meta)?;
        serde_json::to_vec(&payload).map_err(SerializeError::Serde)
    }

    fn to_payload(
        &self,
        canonical_payload: &CanonicalHookFirePayload,
        openclaw_meta: &OpenClawHookMeta,
    ) -> Result<CodexStdinPayload, SerializeError> {
        required("session_id", &canonical_payload.session_id)?;
        required("received_at", &canonical_payload.received_at)?;
        required("relay_id", &openclaw_meta.relay_id)?;
        required("run_id", &openclaw_meta.run_id)?;

        let event = OpenClawHookEventNameMap::to_openclaw(canonical_payload.event);

        Ok(CodexStdinPayload {
            provider: openclaw_meta.provider,
            relay_id: openclaw_meta.relay_id.clone(),
            event,
            native_event_name: Some(event.native_name()),
            agent_id: openclaw_meta.agent_id.clone(),
            session_id: canonical_payload.session_id.clone(),
            session_key: openclaw_meta.session_key.clone(),
            run_id: openclaw_meta.run_id.clone(),
            cwd: canonical_payload.cwd.clone(),
            model: openclaw_meta.model.clone(),
            turn_id: openclaw_meta.turn_id.clone(),
            transcript_path: openclaw_meta.transcript_path.clone(),
            permission_mode: openclaw_meta.permission_mode.clone(),
            stop_hook_active: openclaw_meta.stop_hook_active,
            last_assistant_message: openclaw_meta.last_assistant_message.clone(),
            tool_name: canonical_payload.tool_name.clone(),
            tool_use_id: openclaw_meta.tool_use_id.clone(),
            raw_payload: canonical_payload.raw_payload.clone(),
            received_at: canonical_payload.received_at.clone(),
        })
    }
}

fn required(field: &'static str, value: &str) -> Result<(), SerializeError> {
    if value.trim().is_empty() {
        Err(SerializeError::MissingField(field))
    } else {
        Ok(())
    }
}
