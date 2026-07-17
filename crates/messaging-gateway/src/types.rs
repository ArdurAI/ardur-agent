//! The gateway's shared value types: the channel/participant handles, the
//! message-body taxonomy, the addressing target, and the outgoing/incoming/
//! receipt envelopes a delivery is expressed in.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// `CapTokenRef` is owned by §1.0 (the runtime authorizes a turn with it); an
// outgoing message carries the same handle, so re-use rather than redefine.
use ardur_runtime::CapTokenRef;

// A Unix timestamp in milliseconds since the epoch. Re-exported from the
// workspace-canonical `ardur-core-types` so receipts and gateways stamp time
// from a single `UnixTsMillis` newtype (the collapse this crate's alias long
// anticipated). Its wire form is the bare millisecond integer, so message
// received/delivered timestamps are unchanged.
pub use ardur_core_types::UnixTsMillis;

/// Identifier of a delivery channel — a URI-shaped string such as
/// `"in-process://default"` or `"slack://workspace/channel"`. The
/// [`GatewayRegistry`](crate::GatewayRegistry) key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(pub String);

/// Reference to an individual user (the addressable recipient of a direct
/// message, or the subject of a [`MessageBody::Mention`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserRef(pub String);

/// Reference to a broadcast channel within a provider (e.g. a Slack channel).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelRef(pub String);

/// Reference to a reply thread a message is addressed into.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadRef(pub String);

/// Identifier of the thread an [`IncomingMessage`] was observed in, when the
/// channel supports threading.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadId(pub String);

/// Reference to the originator of an [`IncomingMessage`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SenderRef(pub String);

/// The content of a message.
///
/// Serialized **adjacently tagged** (`{"kind": ..., "data": ...}`): the
/// newtype variants wrap a bare `String`, which serde's *internally* tagged
/// representation cannot encode (a string is not a map). Adjacent tagging
/// represents every variant shape — newtype and struct alike — so each variant
/// round-trips through JSON unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum MessageBody {
    /// Plain, unformatted text.
    Text(String),
    /// Markdown-formatted text, rendered by the receiving channel.
    Markdown(String),
    /// A named binary attachment carried inline.
    // TODO §4.0 Phase 2: large attachments should be offloaded to S3 and
    // referenced by URL rather than inlined as bytes.
    Attachment {
        /// Display file name.
        name: String,
        /// IANA media type, e.g. `"text/plain"`.
        mime_type: String,
        /// Raw attachment bytes.
        bytes: Vec<u8>,
    },
    /// A directed mention of a user, alongside the accompanying text.
    Mention {
        /// The user being mentioned.
        user_ref: UserRef,
        /// The text accompanying the mention.
        body: String,
    },
}

/// Where an [`OutgoingMessage`] is addressed.
///
/// Serialized adjacently tagged (`{"kind": ..., "data": ...}`) for the same
/// reason as [`MessageBody`] — the variants wrap newtypes over `String`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum MessageTarget {
    /// A direct message to a single user.
    User(UserRef),
    /// A post to a broadcast channel.
    Channel(ChannelRef),
    /// A reply into an existing thread.
    Thread(ThreadRef),
}

/// A message handed to a gateway for delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingMessage {
    /// Caller-minted id correlating this send to its [`MessageReceipt`].
    pub message_id: Uuid,
    /// The channel the message is delivered through.
    pub channel_id: ChannelId,
    /// Who/where the message is addressed to.
    pub target: MessageTarget,
    /// The message content.
    pub body: MessageBody,
    /// The capability token authorizing this send (§1.0 / §11.14).
    pub cap_token: CapTokenRef,
    /// The message this is a threaded reply to, if any.
    pub parent_message_id: Option<Uuid>,
}

/// A message observed arriving on a channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingMessage {
    /// Id of the received message.
    pub message_id: Uuid,
    /// The channel the message arrived on.
    pub channel_id: ChannelId,
    /// Who sent it.
    pub sender: SenderRef,
    /// The message content.
    pub body: MessageBody,
    /// When it was received (Unix ms).
    pub received_at: UnixTsMillis,
    /// The thread it belongs to, when the channel supports threading.
    pub thread_id: Option<ThreadId>,
}

/// Proof that a gateway accepted an [`OutgoingMessage`] for delivery.
///
/// [`receipt_id`](Self::receipt_id) is the bridge to the §11.14 receipt chain:
/// the signed execution receipt references this id to bind the delivery to the
/// run that produced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageReceipt {
    /// The channel the message was delivered to.
    pub delivered_to: ChannelId,
    /// When delivery was accepted (Unix ms).
    pub delivered_at: UnixTsMillis,
    /// The provider's own message id, once a real backend assigns one.
    // TODO §4.0 Phase 2: populate from the Slack/Signal/Discord send response.
    pub provider_message_id: Option<String>,
    /// Id linking this delivery into the §11.14 receipt chain.
    pub receipt_id: Uuid,
}
