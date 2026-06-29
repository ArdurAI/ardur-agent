#[derive(Debug, thiserror::Error)]
pub enum StandingGoalError {
    #[error("goal not found: {0}")]
    NotFound(String),
    #[error("goal already exists: {0}")]
    AlreadyExists(String),
    #[error("invalid goal: {0}")]
    Invalid(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, StandingGoalError>;
