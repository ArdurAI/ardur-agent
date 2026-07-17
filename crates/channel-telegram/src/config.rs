//! [`TelegramConfig`] — the adapter's startup configuration, with a [`builder`]
//! and a [`from_env`] reader.
//!
//! [`builder`]: TelegramConfig::builder
//! [`from_env`]: TelegramConfig::from_env

use secrecy::SecretString;

use crate::error::TelegramError;

/// Environment variable names [`TelegramConfig::from_env`] reads.
pub(crate) const ENV_BOT_TOKEN: &str = "TELEGRAM_BOT_TOKEN";
pub(crate) const ENV_ALLOWED_CHATS: &str = "TELEGRAM_ALLOWED_CHATS";

/// The resolved Telegram bot configuration.
///
/// Build it with [`TelegramConfig::builder`] (explicit values) or
/// [`TelegramConfig::from_env`] (the `TELEGRAM_*` variables). The bot token is
/// held as a [`SecretString`] so it never leaks through `Debug`/logs.
#[derive(Clone)]
pub struct TelegramConfig {
    /// The bot's Bot-API token (required) — the `<id>:<secret>` credential.
    pub bot_token: SecretString,
    /// Chat-id allowlist. Deny-by-default (ARD-475): empty means drop *all*
    /// chats; otherwise inbound messages from chats not in this list are
    /// dropped. The operator must explicitly list the chats the bot may read.
    /// Telegram chat ids are signed (`i64`): negative for groups/supergroups,
    /// positive for private chats.
    pub allowed_chat_ids: Vec<i64>,
}

impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `bot_token` is deliberately omitted — `SecretString` redacts it, but
        // spelling that out keeps the redaction obvious at the struct level.
        f.debug_struct("TelegramConfig")
            .field("bot_token", &"<redacted>")
            .field("allowed_chat_ids", &self.allowed_chat_ids)
            .finish()
    }
}

impl TelegramConfig {
    /// Start a [`TelegramConfigBuilder`] from the required bot token.
    #[must_use]
    pub fn builder(bot_token: impl Into<String>) -> TelegramConfigBuilder {
        TelegramConfigBuilder {
            bot_token: bot_token.into(),
            allowed_chat_ids: Vec::new(),
        }
    }

    /// Read the configuration from the `TELEGRAM_*` environment variables.
    ///
    /// Required: `TELEGRAM_BOT_TOKEN`. Optional: `TELEGRAM_ALLOWED_CHATS`
    /// (comma-separated chat ids; empty = all).
    ///
    /// # Errors
    /// [`TelegramError::MissingEnvVar`] when the bot token is unset, or
    /// [`TelegramError::InvalidChatId`] when an allowlist entry fails to parse.
    pub fn from_env() -> Result<Self, TelegramError> {
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
    pub fn from_source<F>(get: F) -> Result<Self, TelegramError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let bot_token = get(ENV_BOT_TOKEN)
            .ok_or_else(|| TelegramError::MissingEnvVar(ENV_BOT_TOKEN.to_owned()))?;
        let allowed_chat_ids = parse_allowed_chats(get(ENV_ALLOWED_CHATS).as_deref())?;

        Ok(Self {
            bot_token: SecretString::from(bot_token),
            allowed_chat_ids,
        })
    }

    /// Whether `chat_id` is permitted: true when the allowlist is empty
    /// (all chats) or contains the id.
    #[must_use]
    pub fn chat_allowed(&self, chat_id: i64) -> bool {
        self.allowed_chat_ids.is_empty() || self.allowed_chat_ids.contains(&chat_id)
    }
}

/// The incremental builder returned by [`TelegramConfig::builder`].
#[derive(Clone, Debug)]
pub struct TelegramConfigBuilder {
    bot_token: String,
    allowed_chat_ids: Vec<i64>,
}

impl TelegramConfigBuilder {
    /// Set the chat-id allowlist (empty = all chats).
    #[must_use]
    pub fn allowed_chat_ids(mut self, ids: Vec<i64>) -> Self {
        self.allowed_chat_ids = ids;
        self
    }

    /// Finalize the configuration.
    ///
    /// # Errors
    /// [`TelegramError::MissingField`] when `bot_token` is empty.
    pub fn build(self) -> Result<TelegramConfig, TelegramError> {
        if self.bot_token.is_empty() {
            return Err(TelegramError::MissingField("bot_token".to_owned()));
        }
        Ok(TelegramConfig {
            bot_token: SecretString::from(self.bot_token),
            allowed_chat_ids: self.allowed_chat_ids,
        })
    }
}

/// Parse the `TELEGRAM_ALLOWED_CHATS` value: comma-separated `i64` ids, each
/// entry trimmed, blanks dropped. `None` (or all-blank) yields an empty list =
/// "all chats".
///
/// # Errors
/// [`TelegramError::InvalidChatId`] for the first entry that is not an `i64`.
pub fn parse_allowed_chats(raw: Option<&str>) -> Result<Vec<i64>, TelegramError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<i64>()
                .map_err(|_| TelegramError::InvalidChatId(s.to_owned()))
        })
        .collect()
}
