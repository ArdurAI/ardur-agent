use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config key not found: {0}")]
    KeyNotFound(String),
    #[error("invalid config value: {0}")]
    InvalidValue(String),
    #[error("profile not found: {0}")]
    ProfileNotFound(String),
    #[error("schema validation failed: {0}")]
    SchemaValidationFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ConfigError>;
