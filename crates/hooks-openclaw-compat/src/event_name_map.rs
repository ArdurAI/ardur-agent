use serde::{Deserialize, Serialize};

/// OpenClaw/codex hook event names, serialized in the codex snake_case form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenClawCodexEventName {
    /// Fired before a tool call is dispatched.
    PreToolUse,
    /// Fired after a tool call completes.
    PostToolUse,
    /// Fired when a permission decision is requested.
    PermissionRequest,
    /// Fired before the agent finalizes a turn.
    BeforeAgentFinalize,
}

impl OpenClawCodexEventName {
    /// Stable snake_case codex wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::PermissionRequest => "permission_request",
            Self::BeforeAgentFinalize => "before_agent_finalize",
        }
    }

    /// The PascalCase native hook event name OpenClaw places in payloads.
    #[must_use]
    pub const fn native_name(self) -> OpenClawNativeEventName {
        match self {
            Self::PreToolUse => OpenClawNativeEventName::PreToolUse,
            Self::PostToolUse => OpenClawNativeEventName::PostToolUse,
            Self::PermissionRequest => OpenClawNativeEventName::PermissionRequest,
            Self::BeforeAgentFinalize => OpenClawNativeEventName::BeforeAgentFinalize,
        }
    }
}

impl std::fmt::Display for OpenClawCodexEventName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// OpenClaw native hook event names, serialized in PascalCase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OpenClawNativeEventName {
    /// `PreToolUse`.
    PreToolUse,
    /// `PostToolUse`.
    PostToolUse,
    /// `PermissionRequest`.
    PermissionRequest,
    /// `BeforeAgentFinalize`.
    BeforeAgentFinalize,
}

impl OpenClawNativeEventName {
    /// Stable PascalCase native hook event name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::BeforeAgentFinalize => "BeforeAgentFinalize",
        }
    }

    /// The matching codex event name.
    #[must_use]
    pub const fn codex_event(self) -> OpenClawCodexEventName {
        match self {
            Self::PreToolUse => OpenClawCodexEventName::PreToolUse,
            Self::PostToolUse => OpenClawCodexEventName::PostToolUse,
            Self::PermissionRequest => OpenClawCodexEventName::PermissionRequest,
            Self::BeforeAgentFinalize => OpenClawCodexEventName::BeforeAgentFinalize,
        }
    }
}

impl std::fmt::Display for OpenClawNativeEventName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Ardur canonical hook event names used at the adapter boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalHookEventName {
    /// Ardur canonical pre-tool event.
    PreToolCall,
    /// Ardur canonical post-tool event.
    PostToolCall,
    /// Ardur canonical pre-approval observer event.
    PreApprovalRequest,
    /// Ardur canonical child-agent stop/finalize event.
    SubagentStop,
}

impl CanonicalHookEventName {
    /// Stable snake_case Ardur canonical event name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreToolCall => "pre_tool_call",
            Self::PostToolCall => "post_tool_call",
            Self::PreApprovalRequest => "pre_approval_request",
            Self::SubagentStop => "subagent_stop",
        }
    }
}

impl std::fmt::Display for CanonicalHookEventName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Frozen bidirectional table between OpenClaw codex names and Ardur names.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenClawHookEventNameMap;

impl OpenClawHookEventNameMap {
    /// Map an OpenClaw/codex event into Ardur's canonical hook vocabulary.
    #[must_use]
    pub const fn to_canonical(event: OpenClawCodexEventName) -> CanonicalHookEventName {
        match event {
            OpenClawCodexEventName::PreToolUse => CanonicalHookEventName::PreToolCall,
            OpenClawCodexEventName::PostToolUse => CanonicalHookEventName::PostToolCall,
            OpenClawCodexEventName::PermissionRequest => CanonicalHookEventName::PreApprovalRequest,
            OpenClawCodexEventName::BeforeAgentFinalize => CanonicalHookEventName::SubagentStop,
        }
    }

    /// Map an Ardur canonical event into the OpenClaw/codex compatibility name.
    #[must_use]
    pub const fn to_openclaw(event: CanonicalHookEventName) -> OpenClawCodexEventName {
        match event {
            CanonicalHookEventName::PreToolCall => OpenClawCodexEventName::PreToolUse,
            CanonicalHookEventName::PostToolCall => OpenClawCodexEventName::PostToolUse,
            CanonicalHookEventName::PreApprovalRequest => OpenClawCodexEventName::PermissionRequest,
            CanonicalHookEventName::SubagentStop => OpenClawCodexEventName::BeforeAgentFinalize,
        }
    }
}
