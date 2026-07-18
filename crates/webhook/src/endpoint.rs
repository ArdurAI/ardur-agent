//! Outbound endpoint registry domain (§9.7).
//!
//! A registered endpoint names *where* an emit goes and *how* it is signed.
//! The HMAC secret itself is never stored in the registry: only the name of
//! the environment variable that holds it (`secret_env`) is persisted, so the
//! durable store never contains plaintext key material.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::opstore::Identified;

/// The default HTTP method for an emit.
pub const DEFAULT_METHOD: &str = "POST";
/// The default signature header (per ADR-Phase3-215).
pub const DEFAULT_SIGNATURE_HEADER: &str = "X-Ardur-Webhook-Signature";

/// A registered outbound endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundEndpoint {
    /// Stable endpoint id (UUIDv7).
    pub id: String,
    /// Operator-facing name.
    pub name: String,
    /// Destination URL.
    pub url: String,
    /// HTTP method (`POST` default).
    pub method: String,
    /// Name of the environment variable holding the HMAC secret. The secret
    /// value is never persisted here.
    pub secret_env: String,
    /// Header the signature is placed in.
    pub signature_header: String,
    /// Fingerprint of the owning cap-token holder.
    pub owner_fingerprint: String,
    /// When the endpoint was registered.
    pub registered_at: DateTime<Utc>,
    /// When the endpoint was last updated.
    pub updated_at: DateTime<Utc>,
    /// Whether the endpoint is revoked (soft-delete).
    pub revoked: bool,
    /// Status of the most recent emit/probe, if any.
    pub last_status: Option<u16>,
    /// When the endpoint was last emitted to.
    pub last_attempt_at: Option<DateTime<Utc>>,
}

impl Identified for OutboundEndpoint {
    fn id(&self) -> &str {
        &self.id
    }
}

/// Fields required to register a new endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointRegistration {
    /// Operator-facing name.
    pub name: String,
    /// Destination URL.
    pub url: String,
    /// HTTP method (defaults to `POST` when empty).
    pub method: Option<String>,
    /// Environment-variable name holding the HMAC secret.
    pub secret_env: String,
    /// Signature header (defaults to [`DEFAULT_SIGNATURE_HEADER`] when empty).
    pub signature_header: Option<String>,
}

/// Mutable fields of an endpoint. Signing scheme + secret-env are mutable via
/// update; the endpoint id and owner are not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndpointUpdate {
    /// New URL.
    pub url: Option<String>,
    /// New method.
    pub method: Option<String>,
    /// New secret-env name.
    pub secret_env: Option<String>,
    /// New signature header.
    pub signature_header: Option<String>,
}
