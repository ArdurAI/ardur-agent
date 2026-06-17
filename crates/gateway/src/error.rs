use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("channel not found: {0}")]
    ChannelNotFound(String),
    #[error("message not found: {0}")]
    MessageNotFound(String),
    #[error("routing failed: {0}")]
    RoutingFailed(String),
    #[error("gateway not initialized")]
    NotInitialized,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, GatewayError>;
