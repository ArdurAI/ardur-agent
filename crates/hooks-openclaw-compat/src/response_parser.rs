use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Canonical hook response envelope used by the compatibility boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookResponseEnvelope {
    /// No action requested.
    NoOp,
    /// Explicit allow decision.
    Allow,
    /// Block the triggering action with a human-readable reason.
    Block {
        /// Human-readable block reason.
        reason: String,
    },
}

/// Codex response shape that matched during parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodexResponseShape {
    /// `{hookSpecificOutput: {hookEventName: "PreToolUse", permissionDecision: "deny", ...}}`
    HookSpecificOutputPreToolUse,
    /// `{decision: "block", reason}`.
    BeforeAgentFinalizeRevise,
    /// `{continue: false, stopReason}`.
    BeforeAgentFinalizeStop,
    /// `{hookSpecificOutput: {hookEventName: "PermissionRequest", decision: {behavior, message}}}`.
    HookSpecificOutputPermissionRequest,
}

/// OpenClaw permission decision behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    /// Allow the permission request.
    Allow,
    /// Deny the permission request.
    Deny,
}

/// Permission decision details parsed from an OpenClaw response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenClawPermissionDecision {
    /// Decision behavior.
    pub behavior: PermissionBehavior,
    /// Optional decision message.
    pub message: Option<String>,
}

/// Parser warning recorded for audit/receipt annotation by runtime wiring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenClawResponseWarning {
    /// OpenClaw `PermissionRequest` returned an allow/deny decision, but Ardur
    /// permission hooks are observer-only, so the decision was downgraded to
    /// NoOp while preserving the original intent for audit.
    PermissionDecisionDowngraded {
        /// Original OpenClaw behavior.
        behavior: PermissionBehavior,
        /// Original OpenClaw message, if provided.
        message: Option<String>,
    },
    /// Valid JSON did not match any known codex hook response shape.
    UnknownShape,
}

/// Parsed response plus normalized envelope and optional audit warning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedOpenClawResponse {
    /// Normalized hook response.
    pub envelope: HookResponseEnvelope,
    /// Codex shape matched by the parser.
    pub source_shape: Option<CodexResponseShape>,
    /// Permission decision details when a PermissionRequest shape matched.
    pub permission_decision: Option<OpenClawPermissionDecision>,
    /// Soft warning for receipt annotation/debug logging.
    pub warning: Option<OpenClawResponseWarning>,
}

impl ParsedOpenClawResponse {
    /// Construct a NoOp parse result.
    #[must_use]
    pub fn noop() -> Self {
        Self {
            envelope: HookResponseEnvelope::NoOp,
            source_shape: None,
            permission_decision: None,
            warning: None,
        }
    }

    fn unknown_shape() -> Self {
        Self {
            warning: Some(OpenClawResponseWarning::UnknownShape),
            ..Self::noop()
        }
    }
}

/// Parser failure surface.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// JSON parse failed.
    #[error("invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

/// Closed trait for OpenClaw/codex stdout response parsers.
pub trait OpenClawResponseParser: crate::sealed::Sealed + Send + Sync + 'static {
    /// Parse stdout bytes from an OpenClaw/codex hook process.
    fn parse(&self, stdout: &[u8]) -> Result<ParsedOpenClawResponse, ParseError>;
}

/// Default parser for the five codex response shapes in §9.6.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultOpenClawResponseParser;

impl crate::sealed::Sealed for DefaultOpenClawResponseParser {}

impl OpenClawResponseParser for DefaultOpenClawResponseParser {
    fn parse(&self, stdout: &[u8]) -> Result<ParsedOpenClawResponse, ParseError> {
        let trimmed = trim_ascii(stdout);
        if trimmed.is_empty() {
            return Ok(ParsedOpenClawResponse::noop());
        }

        let value: Value = serde_json::from_slice(trimmed).map_err(ParseError::InvalidJson)?;

        if let Some(parsed) = parse_hook_specific_output(&value) {
            return Ok(parsed);
        }

        if string_field(&value, "decision").is_some_and(|decision| decision == "block") {
            return Ok(ParsedOpenClawResponse {
                envelope: HookResponseEnvelope::Block {
                    reason: string_field(&value, "reason")
                        .filter(|reason| !reason.trim().is_empty())
                        .unwrap_or("Blocked by OpenClaw")
                        .to_string(),
                },
                source_shape: Some(CodexResponseShape::BeforeAgentFinalizeRevise),
                permission_decision: None,
                warning: None,
            });
        }

        if value
            .get("continue")
            .and_then(Value::as_bool)
            .is_some_and(|should_continue| !should_continue)
        {
            return Ok(ParsedOpenClawResponse {
                envelope: HookResponseEnvelope::Block {
                    reason: string_field(&value, "stopReason")
                        .filter(|reason| !reason.trim().is_empty())
                        .unwrap_or("Stopped by OpenClaw")
                        .to_string(),
                },
                source_shape: Some(CodexResponseShape::BeforeAgentFinalizeStop),
                permission_decision: None,
                warning: None,
            });
        }

        Ok(ParsedOpenClawResponse::unknown_shape())
    }
}

fn parse_hook_specific_output(value: &Value) -> Option<ParsedOpenClawResponse> {
    let output = value.get("hookSpecificOutput")?;
    match string_field(output, "hookEventName")? {
        "PreToolUse" if string_field(output, "permissionDecision") == Some("deny") => {
            Some(ParsedOpenClawResponse {
                envelope: HookResponseEnvelope::Block {
                    reason: string_field(output, "permissionDecisionReason")
                        .filter(|reason| !reason.trim().is_empty())
                        .unwrap_or("Denied by OpenClaw")
                        .to_string(),
                },
                source_shape: Some(CodexResponseShape::HookSpecificOutputPreToolUse),
                permission_decision: None,
                warning: None,
            })
        }
        "PermissionRequest" => Some(parse_permission_request(output)),
        _ => Some(ParsedOpenClawResponse::unknown_shape()),
    }
}

fn parse_permission_request(output: &Value) -> ParsedOpenClawResponse {
    let decision = output.get("decision");
    let Some(behavior) = decision
        .and_then(|decision| string_field(decision, "behavior"))
        .and_then(parse_permission_behavior)
    else {
        return ParsedOpenClawResponse::unknown_shape();
    };
    let message = decision
        .and_then(|decision| string_field(decision, "message"))
        .filter(|message| !message.trim().is_empty())
        .map(ToOwned::to_owned);
    let permission_decision = OpenClawPermissionDecision {
        behavior,
        message: message.clone(),
    };

    match behavior {
        PermissionBehavior::Allow => ParsedOpenClawResponse {
            envelope: HookResponseEnvelope::NoOp,
            source_shape: Some(CodexResponseShape::HookSpecificOutputPermissionRequest),
            permission_decision: Some(permission_decision),
            warning: Some(OpenClawResponseWarning::PermissionDecisionDowngraded {
                behavior,
                message,
            }),
        },
        PermissionBehavior::Deny => ParsedOpenClawResponse {
            envelope: HookResponseEnvelope::NoOp,
            source_shape: Some(CodexResponseShape::HookSpecificOutputPermissionRequest),
            permission_decision: Some(permission_decision),
            warning: Some(OpenClawResponseWarning::PermissionDecisionDowngraded {
                behavior,
                message,
            }),
        },
    }
}

fn parse_permission_behavior(behavior: &str) -> Option<PermissionBehavior> {
    match behavior {
        "allow" => Some(PermissionBehavior::Allow),
        "deny" => Some(PermissionBehavior::Deny),
        _ => None,
    }
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|position| position + 1)
        .unwrap_or(start);
    &bytes[start..end]
}
