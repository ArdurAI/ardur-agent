//! [`SlackAdapter`] — the Slack backend for the §4.0 [`MessagingGateway`]
//! contract.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use parking_lot::Mutex;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use ardur_messaging_gateway::{
    ChannelId, GatewayError, IncomingMessage, MessageBody, MessageReceipt, MessageTarget,
    MessagingGateway, OutgoingMessage, UnixTsMillis,
};

use crate::error::SlackError;
use crate::event::{
    EventEnvelope, REPLAY_WINDOW_SECONDS, SlackEvent, SlackHeaders, message_to_incoming,
};

type HmacSha256 = Hmac<Sha256>;

/// Slack's production Web-API base. Overridable via
/// [`SlackAdapter::with_base_url`] so tests can point the adapter at a mock.
const DEFAULT_BASE_URL: &str = "https://slack.com/api";

/// The deserialized `chat.postMessage` response (the fields Phase 1 reads).
#[derive(Debug, Deserialize)]
struct PostMessageResponse {
    ok: bool,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// A Slack channel adapter: sends via `chat.postMessage` and verifies + parses
/// inbound Events-API requests.
///
/// Phase 1 sends with a bot token and verifies inbound requests with the app's
/// signing secret. OAuth, threading, attachment offload, and reactions are
/// Phase 2.
pub struct SlackAdapter {
    bot_token: SecretString,
    signing_secret: SecretString,
    app_id: String,
    http: reqwest::Client,
    base_url: String,
    /// Inbound sender (Slack user id) allowlist. Deny-by-default (ARD-475):
    /// an empty set drops every sender; otherwise only listed user ids may
    /// command the bot.
    allowed_senders: HashSet<String>,
    /// Default in-memory duplicate-delivery guard (ARD-481): timestamp skew
    /// stops stale signed requests, while this cache rejects the same valid
    /// Slack signature if it is replayed again inside the freshness window.
    replay_cache: Mutex<HashMap<String, u64>>,
}

impl SlackAdapter {
    /// Build an adapter from an explicit bot token, signing secret, and app id.
    #[must_use]
    pub fn new(bot_token: SecretString, signing_secret: SecretString, app_id: String) -> Self {
        Self {
            bot_token,
            signing_secret,
            app_id,
            http: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            allowed_senders: HashSet::new(),
            replay_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Build an adapter from the environment: `SLACK_BOT_TOKEN`,
    /// `SLACK_SIGNING_SECRET`, `SLACK_APP_ID`.
    ///
    /// # Errors
    /// [`SlackError::MissingEnvVar`] naming the first unset variable.
    pub fn from_env() -> Result<Self, SlackError> {
        let bot_token = read_env("SLACK_BOT_TOKEN")?;
        let signing_secret = read_env("SLACK_SIGNING_SECRET")?;
        let app_id = read_env("SLACK_APP_ID")?;
        // ARD-475: optional comma-separated sender allowlist (deny-by-default —
        // unset/empty drops every sender, so the operator must list the Slack
        // user ids permitted to command the bot).
        let allowed_senders = std::env::var("SLACK_ALLOWED_SENDERS")
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(Self::new(
            SecretString::from(bot_token),
            SecretString::from(signing_secret),
            app_id,
        )
        .with_allowed_senders(allowed_senders))
    }

    /// Override the Web-API base URL (e.g. point at a mock server in tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Restrict inbound messages to senders whose Slack user id is in `senders`
    /// (ARD-475). Deny-by-default: an empty set drops every sender, so the
    /// operator must list the user ids permitted to command the bot.
    #[must_use]
    pub fn with_allowed_senders<I, S>(mut self, senders: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_senders = senders.into_iter().map(Into::into).collect();
        self
    }

    /// The app id this adapter is configured for.
    #[must_use]
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// POST `chat.postMessage` and return the delivered message's `ts`.
    ///
    /// This is the adapter's native, full-fidelity send: errors surface as the
    /// distinct [`SlackError`] variants rather than the coarser [`GatewayError`]
    /// the trait lowers them to.
    ///
    /// # Errors
    /// - [`SlackError::NetworkFailure`] if the HTTP request never completes.
    /// - [`SlackError::RateLimited`] on HTTP 429 or `error: "ratelimited"`,
    ///   carrying the parsed `Retry-After`.
    /// - [`SlackError::Forbidden`] / [`SlackError::Unauthorized`] /
    ///   [`SlackError::ChannelNotFound`] for the named Slack error codes.
    /// - [`SlackError::Upstream`] for any other `ok: false`.
    /// - [`SlackError::ParseError`] if the response is not the expected shape.
    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
        blocks: Option<Value>,
    ) -> Result<String, SlackError> {
        let url = format!("{}/chat.postMessage", self.base_url);
        let mut payload = json!({ "channel": channel, "text": text });
        if let Some(blocks) = blocks {
            payload["blocks"] = blocks;
        }

        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.bot_token.expose_secret())
            .json(&payload)
            .send()
            .await
            .map_err(|e| SlackError::NetworkFailure(e.to_string()))?;

        // Capture Retry-After before the body consumes the response.
        let retry_after_ms = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|secs| secs.saturating_mul(1_000));

        // Slack signals rate limiting with HTTP 429 (often with an empty body),
        // so branch on the status before attempting to parse JSON.
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SlackError::RateLimited {
                retry_after_ms: retry_after_ms.unwrap_or(0),
            });
        }

        let parsed: PostMessageResponse = resp
            .json()
            .await
            .map_err(|e| SlackError::ParseError(e.to_string()))?;

        if parsed.ok {
            return parsed.ts.ok_or_else(|| {
                SlackError::ParseError("ok=true response carried no ts".to_owned())
            });
        }

        Err(match parsed.error.as_deref().unwrap_or("") {
            "not_in_channel" => SlackError::Forbidden,
            "channel_not_found" => SlackError::ChannelNotFound,
            "invalid_auth" => SlackError::Unauthorized,
            "ratelimited" => SlackError::RateLimited {
                retry_after_ms: retry_after_ms.unwrap_or(0),
            },
            other => SlackError::Upstream(other.to_owned()),
        })
    }

    /// POST `chat.update` for an existing message and return the updated message `ts`.
    ///
    /// # Errors
    /// Same transport and Slack API error classes as [`post_message`](Self::post_message).
    pub async fn update_message(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
        blocks: Option<Value>,
    ) -> Result<String, SlackError> {
        let url = format!("{}/chat.update", self.base_url);
        let mut payload = json!({ "channel": channel, "ts": ts, "text": text });
        if let Some(blocks) = blocks {
            payload["blocks"] = blocks;
        }

        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.bot_token.expose_secret())
            .json(&payload)
            .send()
            .await
            .map_err(|e| SlackError::NetworkFailure(e.to_string()))?;

        let retry_after_ms = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|secs| secs.saturating_mul(1_000));

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SlackError::RateLimited {
                retry_after_ms: retry_after_ms.unwrap_or(0),
            });
        }

        let parsed: PostMessageResponse = resp
            .json()
            .await
            .map_err(|e| SlackError::ParseError(e.to_string()))?;

        if parsed.ok {
            return Ok(parsed.ts.unwrap_or_else(|| ts.to_owned()));
        }

        Err(match parsed.error.as_deref().unwrap_or("") {
            "not_in_channel" => SlackError::Forbidden,
            "channel_not_found" => SlackError::ChannelNotFound,
            "invalid_auth" => SlackError::Unauthorized,
            "ratelimited" => SlackError::RateLimited {
                retry_after_ms: retry_after_ms.unwrap_or(0),
            },
            other => SlackError::Upstream(other.to_owned()),
        })
    }

    /// Verify and parse an inbound Events-API request, using the current wall
    /// clock for the replay check.
    ///
    /// # Errors
    /// See [`parse_event_at`](Self::parse_event_at).
    pub fn parse_event(
        &self,
        headers: &SlackHeaders,
        body: &str,
    ) -> Result<SlackEvent, SlackError> {
        self.parse_event_at(headers, body, now_seconds())
    }

    /// Verify and parse an inbound Events-API request against an explicit
    /// `now_unix` (seconds) — the deterministic core
    /// [`parse_event`](Self::parse_event) delegates to.
    ///
    /// Verification order: replay-window check on the timestamp, constant-time
    /// HMAC signature check, duplicate-delivery replay check, then JSON parsing.
    ///
    /// # Errors
    /// - [`SlackError::ParseError`] if the timestamp header or body is malformed.
    /// - [`SlackError::Replay`] if the timestamp skews more than 5 minutes.
    /// - [`SlackError::InvalidSignature`] if the HMAC does not match.
    /// - [`SlackError::UnsupportedEvent`] for any event type other than a
    ///   `message` event or the `url_verification` handshake.
    pub fn parse_event_at(
        &self,
        headers: &SlackHeaders,
        body: &str,
        now_unix: u64,
    ) -> Result<SlackEvent, SlackError> {
        let ts: u64 =
            headers.timestamp.trim().parse().map_err(|_| {
                SlackError::ParseError("invalid request timestamp header".to_owned())
            })?;

        // Replay protection: reject timestamps outside the ±window (skew in
        // either direction — stale or implausibly future).
        let age = now_unix.abs_diff(ts);
        if age > REPLAY_WINDOW_SECONDS {
            return Err(SlackError::Replay { age_seconds: age });
        }

        // Signature: v0=HMAC-SHA256(signing_secret, "v0:{ts}:{body}").
        self.verify_signature(&headers.signature, &headers.timestamp, body)?;

        // ARD-481: Slack's timestamp freshness window alone still accepts the
        // same signed body repeatedly for five minutes. After proving the HMAC
        // is valid, remember the signature and reject duplicates inside the
        // window by default.
        self.reject_duplicate_replay(&headers.signature, ts, now_unix)?;

        let envelope: EventEnvelope = serde_json::from_str(body)
            .map_err(|e| SlackError::ParseError(format!("malformed event body: {e}")))?;

        match envelope.kind.as_str() {
            "url_verification" => {
                let challenge = envelope.challenge.ok_or_else(|| {
                    SlackError::ParseError("url_verification without challenge".to_owned())
                })?;
                Ok(SlackEvent::UrlVerification { challenge })
            }
            "event_callback" => {
                let event = envelope.event.ok_or_else(|| {
                    SlackError::ParseError("event_callback without event".to_owned())
                })?;
                if event.kind != "message" {
                    return Err(SlackError::UnsupportedEvent(event.kind));
                }
                // Loop-prevention: drop messages authored by our own app. (Slack
                // stamps the bot's own messages with `app_id`; `bot_id` flags any
                // bot author.)
                let own_app = event.app_id.as_deref() == Some(self.app_id.as_str());
                if own_app || event.bot_id.is_some() {
                    return Ok(SlackEvent::Ignored);
                }
                // ARD-475: deny-by-default sender allowlist. An empty allowlist
                // drops every sender; otherwise only listed Slack user ids may
                // command the bot. (Slack's HMAC only proves the request came
                // from Slack — any user's message is signed — so this is the
                // control that restricts *which* users can drive the agent.)
                let user = event.user.as_deref().unwrap_or("");
                if !self.allowed_senders.contains(user) {
                    return Ok(SlackEvent::Ignored);
                }
                Ok(SlackEvent::Message(message_to_incoming(
                    &self.app_id,
                    &event,
                )))
            }
            other => Err(SlackError::UnsupportedEvent(other.to_owned())),
        }
    }

    /// Constant-time check of the presented `v0=` signature against the HMAC
    /// computed over the Slack basestring.
    fn verify_signature(
        &self,
        presented: &str,
        timestamp: &str,
        body: &str,
    ) -> Result<(), SlackError> {
        let basestring = format!("v0:{timestamp}:{body}");
        // `new_from_slice` only errors on key-length constraints HMAC does not
        // impose, so it cannot fail here.
        let mut mac = HmacSha256::new_from_slice(self.signing_secret.expose_secret().as_bytes())
            .expect("HMAC accepts a key of any length");
        mac.update(basestring.as_bytes());
        let computed = format!("v0={}", hex::encode(mac.finalize().into_bytes()));

        if computed.as_bytes().ct_eq(presented.as_bytes()).into() {
            Ok(())
        } else {
            Err(SlackError::InvalidSignature)
        }
    }

    /// Reject duplicate signed inbound deliveries within the configured replay
    /// window. Called only after the timestamp and HMAC have already verified,
    /// so attacker-supplied invalid signatures cannot fill the cache.
    fn reject_duplicate_replay(
        &self,
        signature: &str,
        timestamp: u64,
        now_unix: u64,
    ) -> Result<(), SlackError> {
        let mut cache = self.replay_cache.lock();
        cache.retain(|_, seen_ts| now_unix.abs_diff(*seen_ts) <= REPLAY_WINDOW_SECONDS);

        let key = format!("{timestamp}:{signature}");
        if cache.insert(key, timestamp).is_some() {
            return Err(SlackError::Replay {
                age_seconds: now_unix.abs_diff(timestamp),
            });
        }
        Ok(())
    }

    /// Resolve an [`OutgoingMessage`] target into the Slack `channel` argument.
    fn target_channel(target: &MessageTarget) -> Result<String, GatewayError> {
        match target {
            MessageTarget::User(u) => Ok(u.0.clone()),
            MessageTarget::Channel(c) => Ok(c.0.clone()),
            // Threaded delivery (reply via `thread_ts`) is Phase 2.
            MessageTarget::Thread(_) => Err(GatewayError::UnsupportedFeature(
                "slack adapter Phase 1 cannot deliver into a thread".to_owned(),
            )),
        }
    }

    /// Render an [`OutgoingMessage`] body into Slack `text`.
    fn body_text(body: &MessageBody) -> Result<String, GatewayError> {
        match body {
            MessageBody::Text(t) | MessageBody::Markdown(t) => Ok(t.clone()),
            MessageBody::Mention { user_ref, body } => Ok(format!("<@{}> {}", user_ref.0, body)),
            // Inline attachment bytes need the Phase-2 file-upload path.
            MessageBody::Attachment { .. } => Err(GatewayError::UnsupportedFeature(
                "slack adapter Phase 1 cannot send attachments".to_owned(),
            )),
        }
    }
}

/// Current wall-clock time in Unix seconds (saturating to 0 before the epoch).
fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current wall-clock time in Unix milliseconds.
fn now_millis() -> UnixTsMillis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Read a required environment variable, mapping absence to
/// [`SlackError::MissingEnvVar`].
fn read_env(key: &str) -> Result<String, SlackError> {
    std::env::var(key).map_err(|_| SlackError::MissingEnvVar(key.to_owned()))
}

#[async_trait]
impl MessagingGateway for SlackAdapter {
    async fn send_message(&self, msg: OutgoingMessage) -> Result<MessageReceipt, GatewayError> {
        let channel = Self::target_channel(&msg.target)?;
        let text = Self::body_text(&msg.body)?;

        let ts = self
            .post_message(&channel, &text, None)
            .await
            .map_err(|e| e.into_gateway_error(&channel))?;

        Ok(MessageReceipt {
            delivered_to: msg.channel_id,
            delivered_at: now_millis(),
            // Slack's own message id is the `ts` it returned.
            provider_message_id: Some(ts),
            // The §11.14 receipt-chain id reuses the caller's correlation id so
            // the delivery binds to the run that produced it.
            receipt_id: msg.message_id,
        })
    }

    async fn receive(&self) -> Result<IncomingMessage, GatewayError> {
        // Slack pushes inbound traffic to a webhook; there is no long-poll to
        // drain. A caller fronting the Events-API endpoint feeds request bodies
        // to `parse_event` instead.
        Err(GatewayError::UnsupportedFeature(
            "slack delivers inbound events by webhook push; call parse_event on each request"
                .to_owned(),
        ))
    }

    fn channel_id(&self) -> ChannelId {
        ChannelId(format!("slack://{}", self.app_id))
    }

    fn supports_threading(&self) -> bool {
        // Threading (reply via `thread_ts`) is Phase 2.
        false
    }
}
