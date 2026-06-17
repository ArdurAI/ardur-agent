use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum CuratorError {
    #[error("skill not found: {0}")]
    SkillNotFound(String),
    #[error("skill already exists: {0}")]
    SkillAlreadyExists(String),
    #[error("invalid skill manifest: {0}")]
    InvalidManifest(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CuratorError>;
