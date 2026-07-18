//! Inbound webhook trigger registry domain (§9.7).
//!
//! An inbound trigger maps an external POST (matched by path + source, verified
//! by HMAC over the body with an optional replay window) to an action the agent
//! runs. As with outbound endpoints, the HMAC secret is referenced by
//! environment-variable name — never stored in plaintext.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::opstore::Identified;

/// Default replay window for inbound trigger verification.
pub const DEFAULT_REPLAY_WINDOW_SECS: u64 = 300;

/// A registered inbound trigger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundTrigger {
    /// Stable trigger id (UUIDv7).
    pub id: String,
    /// Operator-facing name.
    pub name: String,
    /// Route path the trigger listens on (e.g. `/hooks/github`).
    pub path: String,
    /// Event source label attached to received events.
    pub source: String,
    /// Environment-variable name holding the inbound HMAC secret.
    pub secret_env: String,
    /// The action the trigger dispatches (e.g. a prompt/mission label).
    pub action: String,
    /// Replay window in seconds for timestamped verification.
    pub replay_window_secs: u64,
    /// Fingerprint of the owning cap-token holder.
    pub owner_fingerprint: String,
    /// When the trigger was registered.
    pub registered_at: DateTime<Utc>,
    /// When the trigger was last updated.
    pub updated_at: DateTime<Utc>,
    /// Whether the trigger is enabled.
    pub enabled: bool,
}

impl Identified for InboundTrigger {
    fn id(&self) -> &str {
        &self.id
    }
}

/// Fields required to register a new inbound trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerRegistration {
    /// Operator-facing name.
    pub name: String,
    /// Route path.
    pub path: String,
    /// Event source label.
    pub source: String,
    /// Environment-variable name holding the inbound HMAC secret.
    pub secret_env: String,
    /// Action to dispatch when the trigger fires.
    pub action: String,
    /// Optional replay window override (defaults to
    /// [`DEFAULT_REPLAY_WINDOW_SECS`]).
    pub replay_window_secs: Option<u64>,
}
