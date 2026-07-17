//! Inbound Events-API handling: signature verification, replay protection, and
//! parsing a signed request body into a [`SlackEvent`].

use ardur_messaging_gateway::{
    ChannelId, IncomingMessage, MessageBody, SenderRef, ThreadId, UnixTsMillis,
};
use serde::Deserialize;
use uuid::Uuid;

/// The two Slack request headers the signature check consumes.
///
/// Lifting them into a struct (rather than threading a raw header map through
/// the adapter) keeps [`parse_event`](crate::SlackAdapter::parse_event)
/// transport-agnostic — a caller fronting Slack with axum, hyper, or a Lambda
/// shim populates these two fields however its framework exposes headers.
#[derive(Clone, Debug)]
pub struct SlackHeaders {
    /// `X-Slack-Signature` — the `v0=<hex>` HMAC Slack computed over the
    /// request basestring.
    pub signature: String,
    /// `X-Slack-Request-Timestamp` — Unix seconds, as a string, when Slack sent
    /// the request. Folded into the basestring and checked for replay.
    pub timestamp: String,
}

impl SlackHeaders {
    /// Convenience constructor from the two header values.
    pub fn new(signature: impl Into<String>, timestamp: impl Into<String>) -> Self {
        Self {
            signature: signature.into(),
            timestamp: timestamp.into(),
        }
    }
}

/// The outcome of verifying and parsing an inbound Slack request.
///
/// A single [`IncomingMessage`] return cannot express the `url_verification`
/// handshake (which has no message and must be echoed back) nor a filtered
/// bot-own message (which is verified but intentionally dropped), so the parse
/// result is this three-way enum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlackEvent {
    /// The one-time URL-verification handshake: the caller must echo
    /// `challenge` back in the HTTP response body.
    UrlVerification {
        /// The challenge token to return verbatim.
        challenge: String,
    },
    /// A genuine inbound user message, lifted into the gateway's envelope.
    Message(IncomingMessage),
    /// A verified event that the adapter deliberately drops — currently a
    /// message authored by this app's own bot (loop-prevention).
    Ignored,
}

/// The Slack replay window: requests whose timestamp skews more than this from
/// now are rejected (Slack's documented recommendation is 5 minutes).
pub(crate) const REPLAY_WINDOW_SECONDS: u64 = 5 * 60;

/// Top-level Events-API envelope (the fields Phase 1 reads).
#[derive(Debug, Deserialize)]
pub(crate) struct EventEnvelope {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) challenge: Option<String>,
    pub(crate) event: Option<InnerEvent>,
}

/// The nested `event` object of an `event_callback`.
#[derive(Debug, Deserialize)]
pub(crate) struct InnerEvent {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) user: Option<String>,
    #[serde(default)]
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) channel: Option<String>,
    /// Present on messages posted by any bot integration.
    #[serde(default)]
    pub(crate) bot_id: Option<String>,
    /// The app that authored the message; equals our `app_id` for our own bot.
    #[serde(default)]
    pub(crate) app_id: Option<String>,
    #[serde(default)]
    pub(crate) ts: Option<String>,
    #[serde(default)]
    pub(crate) thread_ts: Option<String>,
}

/// Convert a Slack `ts` (`"1700000000.000200"`, seconds with µs fraction) into
/// Unix milliseconds. Returns `None` if the seconds component does not parse.
pub(crate) fn slack_ts_to_millis(ts: &str) -> Option<u64> {
    let mut parts = ts.splitn(2, '.');
    let secs: u64 = parts.next()?.parse().ok()?;
    // The fractional part is microseconds (6 digits); take the leading 3 as ms.
    let millis_frac: u64 = match parts.next() {
        Some(frac) => {
            let head: String = frac.chars().take(3).collect();
            // Right-pad so "2" → "200" rather than 2.
            format!("{head:0<3}").parse().unwrap_or(0)
        }
        None => 0,
    };
    Some(secs.saturating_mul(1_000).saturating_add(millis_frac))
}

/// Build the [`IncomingMessage`] for a non-bot `message` event. `app_id` is the
/// adapter's configured app id, used to namespace the gateway channel id.
pub(crate) fn message_to_incoming(app_id: &str, ev: &InnerEvent) -> IncomingMessage {
    let channel = ev.channel.as_deref().unwrap_or("unknown");
    IncomingMessage {
        // Slack's own id is the `ts`; the gateway envelope uses a fresh UUID and
        // we keep the `ts` for ordering via `received_at`.
        message_id: Uuid::new_v4(),
        channel_id: ChannelId(format!("slack://{app_id}/{channel}")),
        sender: SenderRef(ev.user.clone().unwrap_or_default()),
        body: MessageBody::Text(ev.text.clone().unwrap_or_default()),
        received_at: UnixTsMillis(
            ev.ts
                .as_deref()
                .and_then(slack_ts_to_millis)
                .unwrap_or_default(),
        ),
        thread_id: ev.thread_ts.clone().map(ThreadId),
    }
}
