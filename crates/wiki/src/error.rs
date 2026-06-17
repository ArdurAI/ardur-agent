#[derive(Debug, thiserror::Error)]
pub enum WikiError {
    #[error("page not found: {0}")]
    PageNotFound(String),
    #[error("invalid page path: {0}")]
    InvalidPath(String),
    #[error("page already exists: {0}")]
    PageAlreadyExists(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, WikiError>;
