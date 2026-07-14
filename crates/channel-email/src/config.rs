//! [`EmailConfig`] — the adapter's startup configuration, with a [`builder`]
//! and a [`from_env`] reader.
//!
//! [`builder`]: EmailConfig::builder
//! [`from_env`]: EmailConfig::from_env

use secrecy::SecretString;

use crate::error::EmailError;

/// Environment variable names [`EmailConfig::from_env`] reads.
pub(crate) const ENV_ADDRESS: &str = "ARDUR_EMAIL_ADDRESS";
pub(crate) const ENV_PASSWORD: &str = "ARDUR_EMAIL_PASSWORD";
pub(crate) const ENV_IMAP_HOST: &str = "ARDUR_EMAIL_IMAP_HOST";
pub(crate) const ENV_IMAP_PORT: &str = "ARDUR_EMAIL_IMAP_PORT";
pub(crate) const ENV_SMTP_HOST: &str = "ARDUR_EMAIL_SMTP_HOST";
pub(crate) const ENV_SMTP_PORT: &str = "ARDUR_EMAIL_SMTP_PORT";
pub(crate) const ENV_ALLOWED_SENDERS: &str = "ARDUR_EMAIL_ALLOWED_SENDERS";
pub(crate) const ENV_POLL_INTERVAL_SECS: &str = "ARDUR_EMAIL_POLL_INTERVAL_SECS";

/// Default IMAPS port (implicit TLS).
const DEFAULT_IMAP_PORT: u16 = 993;
/// Default SMTP submission port (STARTTLS).
const DEFAULT_SMTP_PORT: u16 = 587;
/// Default inbox poll interval, in seconds. Phase 1 polls rather than IDLEs
/// (see the crate docs' adapt-points note), so this trades latency for a
/// simpler, more portable implementation.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 30;

/// The resolved email account configuration.
///
/// Build it with [`EmailConfig::builder`] (explicit values) or
/// [`EmailConfig::from_env`] (the `ARDUR_EMAIL_*` variables). The account
/// password is held as a [`SecretString`] so it never leaks through
/// `Debug`/logs.
#[derive(Clone)]
pub struct EmailConfig {
    /// The account's email address — used as the IMAP/SMTP login and the
    /// `From` header on outgoing mail.
    pub address: String,
    /// The account password (or provider app-password).
    pub password: SecretString,
    /// IMAP server hostname.
    pub imap_host: String,
    /// IMAP server port (implicit TLS). Defaults to 993.
    pub imap_port: u16,
    /// SMTP server hostname.
    pub smtp_host: String,
    /// SMTP server port (STARTTLS submission). Defaults to 587.
    pub smtp_port: u16,
    /// Sender-address allowlist. Deny-by-default (ARD-475, matching the
    /// Telegram/Discord/Matrix adapters): empty means drop *all* inbound
    /// mail; otherwise messages from a `From` address not in this list are
    /// dropped. Compared case-insensitively.
    pub allowed_senders: Vec<String>,
    /// How often the poll loop checks the inbox for unseen messages.
    pub poll_interval_secs: u64,
}

impl std::fmt::Debug for EmailConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `password` is deliberately omitted — `SecretString` redacts it, but
        // spelling that out keeps the redaction obvious at the struct level.
        f.debug_struct("EmailConfig")
            .field("address", &self.address)
            .field("password", &"<redacted>")
            .field("imap_host", &self.imap_host)
            .field("imap_port", &self.imap_port)
            .field("smtp_host", &self.smtp_host)
            .field("smtp_port", &self.smtp_port)
            .field("allowed_senders", &self.allowed_senders)
            .field("poll_interval_secs", &self.poll_interval_secs)
            .finish()
    }
}

impl EmailConfig {
    /// Start an [`EmailConfigBuilder`] from the required account address,
    /// password, IMAP host, and SMTP host.
    #[must_use]
    pub fn builder(
        address: impl Into<String>,
        password: impl Into<String>,
        imap_host: impl Into<String>,
        smtp_host: impl Into<String>,
    ) -> EmailConfigBuilder {
        EmailConfigBuilder {
            address: address.into(),
            password: password.into(),
            imap_host: imap_host.into(),
            imap_port: DEFAULT_IMAP_PORT,
            smtp_host: smtp_host.into(),
            smtp_port: DEFAULT_SMTP_PORT,
            allowed_senders: Vec::new(),
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
        }
    }

    /// Read the configuration from the `ARDUR_EMAIL_*` environment variables.
    ///
    /// Required: `ARDUR_EMAIL_ADDRESS`, `ARDUR_EMAIL_PASSWORD`,
    /// `ARDUR_EMAIL_IMAP_HOST`, `ARDUR_EMAIL_SMTP_HOST`. Optional:
    /// `ARDUR_EMAIL_IMAP_PORT` (default 993), `ARDUR_EMAIL_SMTP_PORT`
    /// (default 587), `ARDUR_EMAIL_ALLOWED_SENDERS` (comma-separated
    /// addresses; empty = deny all), `ARDUR_EMAIL_POLL_INTERVAL_SECS`
    /// (default 30).
    ///
    /// # Errors
    /// [`EmailError::MissingEnvVar`] when a required variable is unset, or
    /// [`EmailError::InvalidPort`] / [`EmailError::InvalidSenderAddress`] when
    /// an optional variable fails to parse.
    pub fn from_env() -> Result<Self, EmailError> {
        // Delegate to the pure getter-based core so it can be unit-tested
        // without mutating process-global env (which is `unsafe` under
        // edition 2024, and this crate is `#![forbid(unsafe_code)]`).
        Self::from_source(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
    }

    /// The pure core of [`from_env`](Self::from_env): resolve the
    /// configuration from a variable getter (`key -> Some(value)` for a set,
    /// non-empty variable, `None` otherwise).
    ///
    /// # Errors
    /// As [`from_env`](Self::from_env).
    pub fn from_source<F>(get: F) -> Result<Self, EmailError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let address =
            get(ENV_ADDRESS).ok_or_else(|| EmailError::MissingEnvVar(ENV_ADDRESS.to_owned()))?;
        let password =
            get(ENV_PASSWORD).ok_or_else(|| EmailError::MissingEnvVar(ENV_PASSWORD.to_owned()))?;
        let imap_host = get(ENV_IMAP_HOST)
            .ok_or_else(|| EmailError::MissingEnvVar(ENV_IMAP_HOST.to_owned()))?;
        let smtp_host = get(ENV_SMTP_HOST)
            .ok_or_else(|| EmailError::MissingEnvVar(ENV_SMTP_HOST.to_owned()))?;

        let imap_port = parse_port(get(ENV_IMAP_PORT).as_deref(), DEFAULT_IMAP_PORT)?;
        let smtp_port = parse_port(get(ENV_SMTP_PORT).as_deref(), DEFAULT_SMTP_PORT)?;
        let allowed_senders = parse_allowed_senders(get(ENV_ALLOWED_SENDERS).as_deref())?;
        let poll_interval_secs = get(ENV_POLL_INTERVAL_SECS)
            .map(|v| {
                v.parse::<u64>()
                    .map_err(|_| EmailError::InvalidPort(v.clone()))
            })
            .transpose()?
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS);

        Ok(Self {
            address,
            password: SecretString::from(password),
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            allowed_senders,
            poll_interval_secs,
        })
    }

    /// Whether `from_address` is permitted: deny-by-default — an empty
    /// allowlist drops every sender; a configured allowlist admits only its
    /// members (case-insensitive).
    #[must_use]
    pub fn sender_allowed(&self, from_address: &str) -> bool {
        let from_lower = from_address.to_ascii_lowercase();
        !self.allowed_senders.is_empty()
            && self
                .allowed_senders
                .iter()
                .any(|s| s.eq_ignore_ascii_case(&from_lower))
    }
}

/// The incremental builder returned by [`EmailConfig::builder`].
#[derive(Clone, Debug)]
pub struct EmailConfigBuilder {
    address: String,
    password: String,
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    allowed_senders: Vec<String>,
    poll_interval_secs: u64,
}

impl EmailConfigBuilder {
    /// Override the IMAP port (default 993).
    #[must_use]
    pub fn imap_port(mut self, port: u16) -> Self {
        self.imap_port = port;
        self
    }

    /// Override the SMTP port (default 587).
    #[must_use]
    pub fn smtp_port(mut self, port: u16) -> Self {
        self.smtp_port = port;
        self
    }

    /// Set the sender-address allowlist (empty = deny all).
    #[must_use]
    pub fn allowed_senders(mut self, senders: Vec<String>) -> Self {
        self.allowed_senders = senders;
        self
    }

    /// Override the inbox poll interval, in seconds (default 30).
    #[must_use]
    pub fn poll_interval_secs(mut self, secs: u64) -> Self {
        self.poll_interval_secs = secs;
        self
    }

    /// Finalize the configuration.
    ///
    /// # Errors
    /// [`EmailError::MissingField`] when a required field is empty.
    pub fn build(self) -> Result<EmailConfig, EmailError> {
        if self.address.is_empty() {
            return Err(EmailError::MissingField("address".to_owned()));
        }
        if self.password.is_empty() {
            return Err(EmailError::MissingField("password".to_owned()));
        }
        if self.imap_host.is_empty() {
            return Err(EmailError::MissingField("imap_host".to_owned()));
        }
        if self.smtp_host.is_empty() {
            return Err(EmailError::MissingField("smtp_host".to_owned()));
        }
        Ok(EmailConfig {
            address: self.address,
            password: SecretString::from(self.password),
            imap_host: self.imap_host,
            imap_port: self.imap_port,
            smtp_host: self.smtp_host,
            smtp_port: self.smtp_port,
            allowed_senders: self.allowed_senders,
            poll_interval_secs: self.poll_interval_secs,
        })
    }
}

/// Parse a port string, falling back to `default` when `raw` is `None`.
///
/// # Errors
/// [`EmailError::InvalidPort`] when `raw` is set but not a `u16`.
fn parse_port(raw: Option<&str>, default: u16) -> Result<u16, EmailError> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    raw.parse::<u16>()
        .map_err(|_| EmailError::InvalidPort(raw.to_owned()))
}

/// Parse the `ARDUR_EMAIL_ALLOWED_SENDERS` value: comma-separated addresses,
/// each entry trimmed, blanks dropped. `None` (or all-blank) yields an empty
/// list = "deny all".
///
/// # Errors
/// [`EmailError::InvalidSenderAddress`] for the first entry that does not
/// contain an `@`.
pub fn parse_allowed_senders(raw: Option<&str>) -> Result<Vec<String>, EmailError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.contains('@') {
                Ok(s.to_ascii_lowercase())
            } else {
                Err(EmailError::InvalidSenderAddress(s.to_owned()))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeEnv(HashMap<&'static str, &'static str>);

    impl FakeEnv {
        fn required() -> Self {
            Self(HashMap::from([
                (ENV_ADDRESS, "bot@example.com"),
                (ENV_PASSWORD, "app-password"),
                (ENV_IMAP_HOST, "imap.example.com"),
                (ENV_SMTP_HOST, "smtp.example.com"),
            ]))
        }

        fn with(mut self, key: &'static str, value: &'static str) -> Self {
            self.0.insert(key, value);
            self
        }

        fn resolve(&self) -> Result<EmailConfig, EmailError> {
            EmailConfig::from_source(|k| self.0.get(k).map(|v| (*v).to_owned()))
        }
    }

    #[test]
    fn config_from_env_defaults() {
        let config = FakeEnv::required().resolve().expect("required vars set");
        assert_eq!(config.imap_port, DEFAULT_IMAP_PORT);
        assert_eq!(config.smtp_port, DEFAULT_SMTP_PORT);
        assert_eq!(config.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
        assert!(
            config.allowed_senders.is_empty(),
            "an unset allowlist means deny all"
        );
        assert!(!config.sender_allowed("anyone@example.com"));
    }

    #[test]
    fn config_from_env_reads_overrides() {
        let config = FakeEnv::required()
            .with(ENV_IMAP_PORT, "1993")
            .with(ENV_SMTP_PORT, "2525")
            .with(ENV_ALLOWED_SENDERS, " Alice@Example.com , bob@example.com")
            .with(ENV_POLL_INTERVAL_SECS, "10")
            .resolve()
            .expect("required vars set");
        assert_eq!(config.imap_port, 1993);
        assert_eq!(config.smtp_port, 2525);
        assert_eq!(config.poll_interval_secs, 10);
        assert!(config.sender_allowed("alice@example.com"));
        assert!(config.sender_allowed("ALICE@EXAMPLE.COM"));
        assert!(config.sender_allowed("bob@example.com"));
        assert!(!config.sender_allowed("carol@example.com"));
    }

    #[test]
    fn config_from_env_requires_address() {
        let err = EmailConfig::from_source(|_| None).expect_err("missing required vars must error");
        assert!(matches!(&err, EmailError::MissingEnvVar(v) if v == ENV_ADDRESS));
    }

    #[test]
    fn invalid_port_is_a_named_error() {
        let err = FakeEnv::required()
            .with(ENV_IMAP_PORT, "not-a-port")
            .resolve()
            .expect_err("non-numeric port must error");
        assert!(matches!(err, EmailError::InvalidPort(v) if v == "not-a-port"));
    }

    #[test]
    fn allowlist_rejects_entries_without_at_sign() {
        let err = parse_allowed_senders(Some("alice@example.com,not-an-address"))
            .expect_err("missing @ is rejected");
        assert!(matches!(err, EmailError::InvalidSenderAddress(v) if v == "not-an-address"));
        assert!(
            parse_allowed_senders(Some("  , ,"))
                .expect("blanks are empty")
                .is_empty()
        );
    }

    #[test]
    fn builder_requires_non_empty_fields() {
        let err = EmailConfig::builder("", "pw", "imap.example.com", "smtp.example.com")
            .build()
            .expect_err("empty address is rejected");
        assert!(matches!(err, EmailError::MissingField(f) if f == "address"));

        let config = EmailConfig::builder(
            "bot@example.com",
            "pw",
            "imap.example.com",
            "smtp.example.com",
        )
        .allowed_senders(vec!["ops@example.com".to_owned()])
        .build()
        .expect("required fields present");
        assert!(config.sender_allowed("ops@example.com"));
        assert!(!config.sender_allowed("other@example.com"));
    }
}
