//! [`MatrixConfig`] — the adapter's startup configuration, with a [`builder`]
//! and an [`from_env`] reader.
//!
//! [`builder`]: MatrixConfig::builder
//! [`from_env`]: MatrixConfig::from_env

use std::path::PathBuf;

use secrecy::SecretString;

use crate::error::MatrixError;

/// Environment variable names [`MatrixConfig::from_env`] reads.
pub(crate) const ENV_HOMESERVER: &str = "MATRIX_HOMESERVER_URL";
pub(crate) const ENV_USER_ID: &str = "MATRIX_USER_ID";
pub(crate) const ENV_ACCESS_TOKEN: &str = "MATRIX_ACCESS_TOKEN";
pub(crate) const ENV_DEVICE_ID: &str = "MATRIX_DEVICE_ID";
pub(crate) const ENV_STATE_DIR: &str = "MATRIX_STATE_DIR";
pub(crate) const ENV_AUTO_JOIN: &str = "MATRIX_AUTO_JOIN_INVITES";
pub(crate) const ENV_ALLOWED_ROOMS: &str = "MATRIX_ALLOWED_ROOMS";

/// The resolved Matrix bot configuration.
///
/// Build it with [`MatrixConfig::builder`] (explicit values) or
/// [`MatrixConfig::from_env`] (the `MATRIX_*` variables). The access token is
/// held as a [`SecretString`] so it never leaks through `Debug`/logs.
#[derive(Clone)]
pub struct MatrixConfig {
    /// Homeserver base URL, e.g. `https://matrix.org` (required).
    pub homeserver_url: String,
    /// The bot's full Matrix user id, e.g. `@ardur-bot:matrix.org` (required).
    pub user_id: String,
    /// The bot's access token (required) — preferred over password auth.
    pub access_token: SecretString,
    /// The device id this session runs under. Recommended: a stable id keeps the
    /// crypto store (and thus decryption keys) consistent across restarts. When
    /// unset, [`DEFAULT_DEVICE_ID`] is used.
    pub device_id: Option<String>,
    /// Directory backing the sqlite state + crypto store. Defaults to
    /// `~/.ardur/matrix-state` (see [`default_state_dir`]).
    pub state_dir: PathBuf,
    /// Whether the bot accepts room invites automatically (default `true`).
    pub auto_join_invites: bool,
    /// Room-id allowlist. Empty means "all rooms"; otherwise inbound messages
    /// from rooms not in this list are dropped.
    pub allowed_rooms: Vec<String>,
}

/// The device id used when [`MatrixConfig::device_id`] is unset. A fixed value
/// (rather than a random one) keeps the crypto store stable across restarts.
pub const DEFAULT_DEVICE_ID: &str = "ARDUR_BOT";

impl std::fmt::Debug for MatrixConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `access_token` is deliberately omitted — `SecretString` redacts it, but
        // spelling that out keeps the redaction obvious at the struct level.
        f.debug_struct("MatrixConfig")
            .field("homeserver_url", &self.homeserver_url)
            .field("user_id", &self.user_id)
            .field("access_token", &"<redacted>")
            .field("device_id", &self.device_id)
            .field("state_dir", &self.state_dir)
            .field("auto_join_invites", &self.auto_join_invites)
            .field("allowed_rooms", &self.allowed_rooms)
            .finish()
    }
}

impl MatrixConfig {
    /// Start a [`MatrixConfigBuilder`] from the three required fields.
    #[must_use]
    pub fn builder(
        homeserver_url: impl Into<String>,
        user_id: impl Into<String>,
        access_token: impl Into<String>,
    ) -> MatrixConfigBuilder {
        MatrixConfigBuilder {
            homeserver_url: homeserver_url.into(),
            user_id: user_id.into(),
            access_token: access_token.into(),
            device_id: None,
            state_dir: None,
            auto_join_invites: true,
            allowed_rooms: Vec::new(),
        }
    }

    /// Read the configuration from the `MATRIX_*` environment variables.
    ///
    /// Required: `MATRIX_HOMESERVER_URL`, `MATRIX_USER_ID`,
    /// `MATRIX_ACCESS_TOKEN`. Optional: `MATRIX_DEVICE_ID`, `MATRIX_STATE_DIR`
    /// (default `~/.ardur/matrix-state`), `MATRIX_AUTO_JOIN_INVITES`
    /// (default `true`), `MATRIX_ALLOWED_ROOMS` (comma-separated; empty = all).
    ///
    /// # Errors
    /// [`MatrixError::MissingEnvVar`] naming the first required variable that is
    /// unset or empty.
    pub fn from_env() -> Result<Self, MatrixError> {
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
    /// [`MatrixError::MissingEnvVar`] naming the first required variable the
    /// getter returns `None` for.
    pub fn from_source<F>(get: F) -> Result<Self, MatrixError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let homeserver_url = get(ENV_HOMESERVER)
            .ok_or_else(|| MatrixError::MissingEnvVar(ENV_HOMESERVER.to_owned()))?;
        let user_id =
            get(ENV_USER_ID).ok_or_else(|| MatrixError::MissingEnvVar(ENV_USER_ID.to_owned()))?;
        let access_token = get(ENV_ACCESS_TOKEN)
            .ok_or_else(|| MatrixError::MissingEnvVar(ENV_ACCESS_TOKEN.to_owned()))?;

        let device_id = get(ENV_DEVICE_ID);
        let state_dir = get(ENV_STATE_DIR).map_or_else(default_state_dir, PathBuf::from);
        let auto_join_invites = get(ENV_AUTO_JOIN).is_none_or(|v| parse_bool(&v));
        let allowed_rooms = parse_allowed_rooms(get(ENV_ALLOWED_ROOMS).as_deref());

        Ok(Self {
            homeserver_url,
            user_id,
            access_token: SecretString::from(access_token),
            device_id,
            state_dir,
            auto_join_invites,
            allowed_rooms,
        })
    }

    /// The device id this session should run under — the configured one, or
    /// [`DEFAULT_DEVICE_ID`] when unset.
    #[must_use]
    pub fn resolved_device_id(&self) -> &str {
        self.device_id.as_deref().unwrap_or(DEFAULT_DEVICE_ID)
    }

    /// Whether `room_id` is permitted: true when the allowlist is empty
    /// (all rooms) or contains the id.
    #[must_use]
    pub fn room_allowed(&self, room_id: &str) -> bool {
        self.allowed_rooms.is_empty() || self.allowed_rooms.iter().any(|r| r == room_id)
    }
}

/// The incremental builder returned by [`MatrixConfig::builder`].
#[derive(Clone, Debug)]
pub struct MatrixConfigBuilder {
    homeserver_url: String,
    user_id: String,
    access_token: String,
    device_id: Option<String>,
    state_dir: Option<PathBuf>,
    auto_join_invites: bool,
    allowed_rooms: Vec<String>,
}

impl MatrixConfigBuilder {
    /// Set the device id (recommended — keeps the crypto store stable).
    #[must_use]
    pub fn device_id(mut self, device_id: impl Into<String>) -> Self {
        self.device_id = Some(device_id.into());
        self
    }

    /// Override the sqlite state-store directory.
    #[must_use]
    pub fn state_dir(mut self, state_dir: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(state_dir.into());
        self
    }

    /// Set whether the bot auto-joins room invites (default `true`).
    #[must_use]
    pub fn auto_join_invites(mut self, auto_join: bool) -> Self {
        self.auto_join_invites = auto_join;
        self
    }

    /// Set the room-id allowlist (empty = all rooms).
    #[must_use]
    pub fn allowed_rooms(mut self, rooms: Vec<String>) -> Self {
        self.allowed_rooms = rooms;
        self
    }

    /// Finalize the configuration.
    ///
    /// # Errors
    /// [`MatrixError::MissingField`] naming the first required field that is
    /// empty (`homeserver_url`, `user_id`, or `access_token`).
    pub fn build(self) -> Result<MatrixConfig, MatrixError> {
        if self.homeserver_url.is_empty() {
            return Err(MatrixError::MissingField("homeserver_url".to_owned()));
        }
        if self.user_id.is_empty() {
            return Err(MatrixError::MissingField("user_id".to_owned()));
        }
        if self.access_token.is_empty() {
            return Err(MatrixError::MissingField("access_token".to_owned()));
        }
        Ok(MatrixConfig {
            homeserver_url: self.homeserver_url,
            user_id: self.user_id,
            access_token: SecretString::from(self.access_token),
            device_id: self.device_id,
            state_dir: self.state_dir.unwrap_or_else(default_state_dir),
            auto_join_invites: self.auto_join_invites,
            allowed_rooms: self.allowed_rooms,
        })
    }
}

/// The default sqlite-store directory: `$HOME/.ardur/matrix-state`, falling back
/// to `./.ardur/matrix-state` when `HOME` is unset.
#[must_use]
pub fn default_state_dir() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".ardur").join("matrix-state")
}

/// Parse the `MATRIX_ALLOWED_ROOMS` value: comma-separated, each entry trimmed,
/// blanks dropped. `None` (or all-blank) yields an empty list = "all rooms".
#[must_use]
pub fn parse_allowed_rooms(raw: Option<&str>) -> Vec<String> {
    raw.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

/// Parse a boolean env value: `true`/`1`/`yes`/`on` (case-insensitive) are true,
/// everything else false.
fn parse_bool(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}
