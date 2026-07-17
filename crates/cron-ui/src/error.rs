//! Error surface for the cron operator UI.

/// Errors raised by the cron operator controller and its adapters.
#[derive(Debug, thiserror::Error)]
pub enum CronUiError {
    /// The referenced cron was not found in the store.
    #[error("cron `{0}` not found")]
    NotFound(String),
    /// A mutate action was refused because the operator's cap-token lacked the
    /// required scope. Read-only by default (ADR-Phase3-278): mutations require
    /// the `cron.ui.mutate` scope; cross-operator views require `cron.ui.admin`.
    #[error("action refused: {0}")]
    Denied(String),
    /// The cron expression could not be parsed as a 5-field cron string.
    #[error("invalid cron expression: {0}")]
    InvalidCron(String),
    /// A persistence (read/write) failure in the durable store.
    #[error("store i/o: {0}")]
    Io(String),
    /// A (de)serialization failure of a stored record.
    #[error("serde: {0}")]
    Serde(String),
    /// Cap-token verification failed (expired, wrong audience, malformed).
    #[error("cap-token: {0}")]
    CapToken(String),
    /// Receipt signing/persistence failed.
    #[error("receipt: {0}")]
    Receipt(String),
}

/// Convenience result alias for the crate.
pub type Result<T> = std::result::Result<T, CronUiError>;
