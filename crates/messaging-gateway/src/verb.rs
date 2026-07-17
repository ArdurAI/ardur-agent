//! Closed per-message operation verbs for the messaging gateway.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::{ChannelId, MessageBody, MessageTarget};

/// Canonical receipt event emitted when a send verb succeeds.
pub const MESSAGE_SENT_EVENT: &str = "channel.message.sent.v1";
/// Canonical receipt event emitted when an edit verb succeeds.
pub const MESSAGE_EDITED_EVENT: &str = "channel.message.edited.v1";
/// Canonical receipt event emitted when a delete verb succeeds.
pub const MESSAGE_DELETED_EVENT: &str = "channel.message.deleted.v1";
/// Canonical receipt event emitted when a react verb succeeds.
pub const MESSAGE_REACTED_EVENT: &str = "channel.message.reacted.v1";
/// Canonical receipt event emitted when a pin verb succeeds.
pub const MESSAGE_PINNED_EVENT: &str = "channel.message.pinned.v1";
/// Canonical receipt event emitted when an unpin verb succeeds.
pub const MESSAGE_UNPINNED_EVENT: &str = "channel.message.unpinned.v1";
/// Canonical receipt event emitted when a forward verb succeeds.
pub const MESSAGE_FORWARDED_EVENT: &str = "channel.message.forwarded.v1";
/// Canonical receipt event emitted when a quote verb succeeds.
pub const MESSAGE_QUOTED_EVENT: &str = "channel.message.quoted.v1";
/// Canonical receipt event emitted when any message verb is refused.
pub const MESSAGE_OP_REFUSED_EVENT: &str = "channel.message.op.refused.v1";

/// Closed enum of per-message operation verbs.
///
/// The variants are intentionally typed instead of represented as strings so
/// adapters cannot invent unreviewed operations at runtime. Future variants
/// should be added here with matching receipt vocabulary and tests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum MessageVerb {
    /// Send a new message with content to the request target.
    Send {
        /// Body to deliver.
        body: MessageBody,
    },
    /// Edit the content of an existing message.
    Edit {
        /// Message being edited.
        target_message_id: Uuid,
        /// Replacement body.
        body: MessageBody,
    },
    /// Delete an existing message.
    Delete {
        /// Message being deleted.
        target_message_id: Uuid,
    },
    /// Add a reaction to an existing message.
    React {
        /// Message receiving the reaction.
        target_message_id: Uuid,
        /// Emoji or provider-native reaction token.
        emoji: String,
    },
    /// Pin an existing message.
    Pin {
        /// Message being pinned.
        target_message_id: Uuid,
    },
    /// Unpin an existing message.
    Unpin {
        /// Message being unpinned.
        target_message_id: Uuid,
    },
    /// Forward an existing message to another target on the same gateway.
    Forward {
        /// Message being forwarded.
        source_message_id: Uuid,
        /// Destination for the forwarded content.
        destination: MessageTarget,
    },
    /// Quote an existing message while emitting new content.
    Quote {
        /// Message being quoted.
        quoted_message_id: Uuid,
        /// Body accompanying the quote.
        body: MessageBody,
    },
}

impl MessageVerb {
    /// Stable lowercase id used in cap-token caveats, policy actions, and logs.
    #[must_use]
    pub fn id(&self) -> &'static str {
        match self {
            Self::Send { .. } => "send",
            Self::Edit { .. } => "edit",
            Self::Delete { .. } => "delete",
            Self::React { .. } => "react",
            Self::Pin { .. } => "pin",
            Self::Unpin { .. } => "unpin",
            Self::Forward { .. } => "forward",
            Self::Quote { .. } => "quote",
        }
    }

    /// Canonical success receipt event for this verb.
    #[must_use]
    pub fn success_event(&self) -> &'static str {
        match self {
            Self::Send { .. } => MESSAGE_SENT_EVENT,
            Self::Edit { .. } => MESSAGE_EDITED_EVENT,
            Self::Delete { .. } => MESSAGE_DELETED_EVENT,
            Self::React { .. } => MESSAGE_REACTED_EVENT,
            Self::Pin { .. } => MESSAGE_PINNED_EVENT,
            Self::Unpin { .. } => MESSAGE_UNPINNED_EVENT,
            Self::Forward { .. } => MESSAGE_FORWARDED_EVENT,
            Self::Quote { .. } => MESSAGE_QUOTED_EVENT,
        }
    }

    /// Whether the verb emits or changes user-visible content.
    #[must_use]
    pub fn emits_content(&self) -> bool {
        matches!(
            self,
            Self::Send { .. } | Self::Edit { .. } | Self::Forward { .. } | Self::Quote { .. }
        )
    }

    /// Whether the verb targets and mutates state for a previously known message.
    #[must_use]
    pub fn mutates_prior_message(&self) -> bool {
        matches!(
            self,
            Self::Edit { .. }
                | Self::Delete { .. }
                | Self::React { .. }
                | Self::Pin { .. }
                | Self::Unpin { .. }
        )
    }
}

/// A gateway request to perform one typed message verb.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageVerbRequest {
    /// Caller-minted id for the verb operation.
    pub operation_id: Uuid,
    /// Channel the verb is issued against.
    pub channel_id: ChannelId,
    /// Default addressing target for verbs that emit content.
    pub target: MessageTarget,
    /// Typed operation to perform.
    pub verb: MessageVerb,
    /// Capability token authorizing this operation.
    pub cap_token: ardur_runtime::CapTokenRef,
    /// Parent message for threaded delivery or reply association, if any.
    pub parent_message_id: Option<Uuid>,
}
