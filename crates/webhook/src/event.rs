use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Normalized webhook event envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebhookEvent {
    /// Unique event identifier (UUIDv7 — time-ordered).
    pub id: Uuid,
    /// UTC timestamp when the event was received or created.
    pub timestamp: DateTime<Utc>,
    /// The event type.
    pub event_type: EventType,
    /// Source identifier (e.g., "slack", "github", "custom").
    pub source: String,
    /// The event payload.
    pub payload: serde_json::Value,
}

impl WebhookEvent {
    /// Create a new event with the current timestamp and a fresh UUIDv7.
    pub fn new(
        event_type: EventType,
        source: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            id: Uuid::now_v7(),
            timestamp: Utc::now(),
            event_type,
            source: source.into(),
            payload,
        }
    }
}

/// Common event type variants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Push,
    PullRequest,
    Issue,
    Comment,
    Deploy,
    Build,
    Custom(String),
}
