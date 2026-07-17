use crate::payload_serializer::{
    CanonicalHookFirePayload, CodexStdinPayload, DefaultOpenClawPayloadSerializer,
    OpenClawHookMeta, OpenClawPayloadSerializer, SerializeError,
};
use crate::response_parser::{
    DefaultOpenClawResponseParser, OpenClawResponseParser, ParseError, ParsedOpenClawResponse,
};

/// Narrow compatibility source that bundles the default serializer and parser.
///
/// This is intentionally not wired into `ardur-lifecycle-hooks` yet. The
/// current crate provides deterministic adapter primitives; runtime hook
/// registration can wrap this source when the lifecycle substrate exposes its
/// external hook-source boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenClawHookSource {
    serializer: DefaultOpenClawPayloadSerializer,
    parser: DefaultOpenClawResponseParser,
}

impl OpenClawHookSource {
    /// Create a source with default serializer and parser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the typed codex stdin payload from canonical hook fields.
    pub fn to_payload(
        &self,
        canonical_payload: &CanonicalHookFirePayload,
        openclaw_meta: &OpenClawHookMeta,
    ) -> Result<CodexStdinPayload, SerializeError> {
        self.serializer.to_payload(canonical_payload, openclaw_meta)
    }

    /// Serialize canonical hook fields into codex stdin bytes.
    pub fn serialize(
        &self,
        canonical_payload: &CanonicalHookFirePayload,
        openclaw_meta: &OpenClawHookMeta,
    ) -> Result<Vec<u8>, SerializeError> {
        self.serializer.serialize(canonical_payload, openclaw_meta)
    }

    /// Parse codex stdout bytes into a normalized hook response.
    pub fn parse_response(&self, stdout: &[u8]) -> Result<ParsedOpenClawResponse, ParseError> {
        self.parser.parse(stdout)
    }
}
