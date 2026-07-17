//! [`DiscordConfig`] — the adapter's startup configuration, with a [`builder`]
//! and a [`from_env`] reader.
//!
//! [`builder`]: DiscordConfig::builder
//! [`from_env`]: DiscordConfig::from_env

use secrecy::SecretString;
use serenity::all::GatewayIntents;

use crate::error::DiscordError;

/// Environment variable names [`DiscordConfig::from_env`] reads.
pub(crate) const ENV_BOT_TOKEN: &str = "DISCORD_BOT_TOKEN";
pub(crate) const ENV_APPLICATION_ID: &str = "DISCORD_APPLICATION_ID";
pub(crate) const ENV_ALLOWED_CHANNELS: &str = "DISCORD_ALLOWED_CHANNELS";

/// The gateway intents Phase 1 subscribes to: guild + direct message events plus
/// the privileged `MESSAGE_CONTENT` intent (so inbound text is non-empty). The
/// privileged `MESSAGE_CONTENT` intent must also be enabled in the Discord
/// developer portal for the bot — see this crate's README.
pub const DEFAULT_INTENTS: GatewayIntents = GatewayIntents::GUILD_MESSAGES
    .union(GatewayIntents::DIRECT_MESSAGES)
    .union(GatewayIntents::MESSAGE_CONTENT);

/// The resolved Discord bot configuration.
///
/// Build it with [`DiscordConfig::builder`] (explicit values) or
/// [`DiscordConfig::from_env`] (the `DISCORD_*` variables). The bot token is
/// held as a [`SecretString`] so it never leaks through `Debug`/logs.
#[derive(Clone)]
pub struct DiscordConfig {
    /// The bot's gateway token (required) — the `Bot <token>` credential.
    pub bot_token: SecretString,
    /// The bot's application id (required). For a bot account this equals the
    /// bot's own user id, so it doubles as the self-echo filter (an inbound
    /// message authored by this id is the bot's own and is dropped).
    pub application_id: u64,
    /// The gateway intents to subscribe to (default [`DEFAULT_INTENTS`]).
    pub intents: GatewayIntents,
    /// Channel-id allowlist. Deny-by-default (ARD-475): empty means drop *all*
    /// channels; otherwise inbound messages from channels not in this list are
    /// dropped. The operator must explicitly list the channels the bot may read.
    pub allowed_channel_ids: Vec<u64>,
}

impl std::fmt::Debug for DiscordConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `bot_token` is deliberately omitted — `SecretString` redacts it, but
        // spelling that out keeps the redaction obvious at the struct level.
        f.debug_struct("DiscordConfig")
            .field("bot_token", &"<redacted>")
            .field("application_id", &self.application_id)
            .field("intents", &self.intents)
            .field("allowed_channel_ids", &self.allowed_channel_ids)
            .finish()
    }
}

impl DiscordConfig {
    /// Start a [`DiscordConfigBuilder`] from the two required fields.
    #[must_use]
    pub fn builder(bot_token: impl Into<String>, application_id: u64) -> DiscordConfigBuilder {
        DiscordConfigBuilder {
            bot_token: bot_token.into(),
            application_id,
            intents: DEFAULT_INTENTS,
            allowed_channel_ids: Vec::new(),
        }
    }

    /// Read the configuration from the `DISCORD_*` environment variables.
    ///
    /// Required: `DISCORD_BOT_TOKEN`, `DISCORD_APPLICATION_ID`. Optional:
    /// `DISCORD_ALLOWED_CHANNELS` (comma-separated channel ids; empty = all).
    ///
    /// # Errors
    /// [`DiscordError::MissingEnvVar`] naming the first required variable that is
    /// unset or empty, or [`DiscordError::InvalidApplicationId`] /
    /// [`DiscordError::InvalidChannelId`] when a numeric value fails to parse.
    pub fn from_env() -> Result<Self, DiscordError> {
        // Delegate to the pure getter-based core so it can be unit-tested without
        // mutating process-global env (which is `unsafe` under edition 2024, and
        // this crate is `#![forbid(unsafe_code)]`).
        Self::from_source(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
    }

    /// The pure core of [`from_env`](Self::from_env): resolve the configuration
    /// from a variable getter (`key -> Some(value)` for a set, non-empty
    /// variable, `None` otherwise).
    ///
    /// # Errors
    /// As [`from_env`](Self::from_env).
    pub fn from_source<F>(get: F) -> Result<Self, DiscordError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let bot_token = get(ENV_BOT_TOKEN)
            .ok_or_else(|| DiscordError::MissingEnvVar(ENV_BOT_TOKEN.to_owned()))?;
        let application_id_raw = get(ENV_APPLICATION_ID)
            .ok_or_else(|| DiscordError::MissingEnvVar(ENV_APPLICATION_ID.to_owned()))?;
        let application_id = application_id_raw
            .parse::<u64>()
            .map_err(|_| DiscordError::InvalidApplicationId(application_id_raw))?;
        let allowed_channel_ids = parse_allowed_channels(get(ENV_ALLOWED_CHANNELS).as_deref())?;

        Ok(Self {
            bot_token: SecretString::from(bot_token),
            application_id,
            intents: DEFAULT_INTENTS,
            allowed_channel_ids,
        })
    }

    /// Whether `channel_id` is permitted: true when the allowlist is empty
    /// (all channels) or contains the id.
    #[must_use]
    pub fn channel_allowed(&self, channel_id: u64) -> bool {
        self.allowed_channel_ids.is_empty() || self.allowed_channel_ids.contains(&channel_id)
    }
}

/// The incremental builder returned by [`DiscordConfig::builder`].
#[derive(Clone, Debug)]
pub struct DiscordConfigBuilder {
    bot_token: String,
    application_id: u64,
    intents: GatewayIntents,
    allowed_channel_ids: Vec<u64>,
}

impl DiscordConfigBuilder {
    /// Override the gateway intents (default [`DEFAULT_INTENTS`]).
    #[must_use]
    pub fn intents(mut self, intents: GatewayIntents) -> Self {
        self.intents = intents;
        self
    }

    /// Set the channel-id allowlist (empty = all channels).
    #[must_use]
    pub fn allowed_channel_ids(mut self, ids: Vec<u64>) -> Self {
        self.allowed_channel_ids = ids;
        self
    }

    /// Finalize the configuration.
    ///
    /// # Errors
    /// [`DiscordError::MissingField`] when `bot_token` is empty or
    /// `application_id` is zero (both required).
    pub fn build(self) -> Result<DiscordConfig, DiscordError> {
        if self.bot_token.is_empty() {
            return Err(DiscordError::MissingField("bot_token".to_owned()));
        }
        if self.application_id == 0 {
            return Err(DiscordError::MissingField("application_id".to_owned()));
        }
        Ok(DiscordConfig {
            bot_token: SecretString::from(self.bot_token),
            application_id: self.application_id,
            intents: self.intents,
            allowed_channel_ids: self.allowed_channel_ids,
        })
    }
}

/// Parse the `DISCORD_ALLOWED_CHANNELS` value: comma-separated `u64` ids, each
/// entry trimmed, blanks dropped. `None` (or all-blank) yields an empty list =
/// "all channels".
///
/// # Errors
/// [`DiscordError::InvalidChannelId`] for the first entry that is not a `u64`.
pub fn parse_allowed_channels(raw: Option<&str>) -> Result<Vec<u64>, DiscordError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<u64>()
                .map_err(|_| DiscordError::InvalidChannelId(s.to_owned()))
        })
        .collect()
}
