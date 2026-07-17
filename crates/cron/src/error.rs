#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("invalid cron expression: {0}")]
    InvalidExpression(String),
    #[error("cron parse error: {0}")]
    Parse(String),
    #[error("no next execution time within the search horizon")]
    NoNextExecution,
    #[error("job not found: {0}")]
    JobNotFound(String),
    #[error("scheduler already running")]
    AlreadyRunning,
    #[error("scheduler not running")]
    NotRunning,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CronError>;
