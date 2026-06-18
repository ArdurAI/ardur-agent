#[derive(Debug, thiserror::Error)]
pub enum HealthError {
    #[error("health check failed: {0}")]
    CheckFailed(String),
    #[error("component not found: {0}")]
    ComponentNotFound(String),
    #[error("diagnostic failed: {0}")]
    DiagnosticFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, HealthError>;
