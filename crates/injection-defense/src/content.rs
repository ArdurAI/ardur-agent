//! The content a filter scans, and where it came from.

use std::path::PathBuf;

use ardur_messaging_gateway::ChannelId;
use ardur_tool_registry::ToolId;

use crate::error::FilterError;

/// Where a piece of scannable content originated. The source is what a future
/// per-source policy (Phase 2) will key its strictness on — content typed
/// directly by the user in the REPL is trusted more than a webhook payload or
/// the nested output of a tool that becomes the next turn's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentSource {
    /// Typed by the user directly in the REPL.
    Direct,
    /// Arrived over a messaging channel (Slack/Signal/etc.).
    Channel(ChannelId),
    /// An external webhook payload, labelled by the webhook's identifier.
    Webhook(String),
    /// The output of a tool that becomes input to the next turn (nested).
    ToolReturn(ToolId),
}

/// A unit of content to scan before it reaches a provider.
#[derive(Debug, Clone, PartialEq)]
pub enum ScannableContent {
    /// A user-authored message and where it came from.
    UserMessage {
        /// The message text.
        text: String,
        /// The message's origin.
        source: ContentSource,
    },
    /// The output of a tool invocation, carried as arbitrary JSON.
    ToolOutput {
        /// The tool that produced the output.
        tool_id: ToolId,
        /// The tool's result value.
        output: serde_json::Value,
    },
    /// The body of a fetched web resource.
    WebFetchResult {
        /// The fetched URL.
        url: String,
        /// The response body.
        body: String,
    },
    /// The contents of a file read into context.
    FileContent {
        /// The file's path.
        path: PathBuf,
        /// The file body.
        body: String,
    },
}

impl ScannableContent {
    /// The text a filter matches its patterns against. Tool outputs are
    /// stringified from their JSON value so structured injections (e.g. a
    /// `{"note": "ignore previous instructions"}` field) are still scanned.
    pub(crate) fn scannable_text(&self) -> Result<String, FilterError> {
        match self {
            ScannableContent::UserMessage { text, .. } => Ok(text.clone()),
            ScannableContent::WebFetchResult { body, .. } => Ok(body.clone()),
            ScannableContent::FileContent { body, .. } => Ok(body.clone()),
            ScannableContent::ToolOutput { output, .. } => serde_json::to_string(output)
                .map_err(|e| FilterError::InvalidInput(format!("tool output not scannable: {e}"))),
        }
    }
}
