//! The approvals surface — the one write-capable operator action this
//! dashboard exposes.
//!
//! `ardur-server` mounts its own admin-bearer-gated decide API
//! (`GET /approvals`, `POST /approvals/{id}/approve`,
//! `POST /approvals/{id}/reject`) directly on the on-disk approvals store the
//! CLI's `ardur approvals` subcommand also uses. Rather than admin-ui reading
//! or writing that store itself — which would give this "read-only"
//! dashboard a second, undocumented write path — it proxies to
//! `ardur-server`'s own API over HTTP, forwarding the configured admin
//! bearer token. `ardur-server` remains the sole writer of its own state;
//! admin-ui never opens the approvals store for write.

use std::time::Duration;

use serde_json::Value;

/// How long to wait for `ardur-server` to respond before giving up.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on the accepted approval-id length, matching `ardur-server`'s own
/// `MAX_APPROVAL_ID_LEN` — ids are validated locally before any network call,
/// so a malformed id can never reach the network.
const MAX_APPROVAL_ID_LEN: usize = 128;

/// Where to reach `ardur-server`'s admin API, and the token to present.
#[derive(Clone)]
pub struct ServerConfig {
    /// Base URL, e.g. `http://127.0.0.1:3000` — no trailing slash assumed.
    base_url: String,
    /// The admin bearer token `ardur-server` was started with
    /// (`ARDUR_ADMIN_BEARER_TOKENS`).
    admin_token: String,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak the token into logs / Debug output.
        f.debug_struct("ServerConfig")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl ServerConfig {
    /// Build from a base URL and admin token.
    #[must_use]
    pub fn new(base_url: impl Into<String>, admin_token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            admin_token: admin_token.into(),
        }
    }
}

/// Whether `id` is safe to interpolate into a `ardur-server` URL path:
/// non-empty, bounded, and drawn only from `[A-Za-z0-9_-]` — the same rule
/// `ardur-server`'s own `valid_approval_id` enforces. Checked here too so a
/// malformed id is rejected locally instead of reaching the network only to
/// be rejected there.
#[must_use]
pub fn valid_approval_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_APPROVAL_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A decision to record against a pending approval card.
pub enum Decision {
    /// Approve the pending action.
    Approve,
    /// Reject it, with an optional human-readable reason.
    Reject {
        /// Why it was rejected. Empty is valid — `ardur-server` treats an
        /// absent/empty reason as "no reason given".
        reason: String,
    },
}

/// Everything that can go wrong proxying to `ardur-server`.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalsError {
    /// The approval id failed the local `[A-Za-z0-9_-]`-bounded check —
    /// never sent to the network.
    #[error("invalid approval id")]
    InvalidId,
    /// The outbound request to `ardur-server` itself failed (connection
    /// refused, timeout, TLS error, ...) — not a decision from the server.
    #[error("request to ardur-server failed: {0}")]
    Transport(String),
    /// `ardur-server` responded, but not with a body we can parse as JSON.
    #[error("ardur-server returned a non-JSON response ({status})")]
    InvalidResponse {
        /// The HTTP status `ardur-server` returned.
        status: u16,
    },
    /// `ardur-server` rejected the admin bearer token (admin-ui is
    /// misconfigured with a stale/wrong token) or has none configured
    /// (fails closed on its side too).
    #[error("ardur-server rejected the configured admin bearer token")]
    Unauthorized,
    /// `ardur-server` returned some other non-2xx status; the body (if any)
    /// is preserved for the caller to relay.
    #[error("ardur-server returned {status}")]
    ServerError {
        /// The HTTP status `ardur-server` returned.
        status: u16,
        /// The response body, if any.
        body: Value,
    },
}

/// List every approval card `ardur-server` currently has on record.
///
/// # Errors
/// See [`ApprovalsError`].
pub async fn list(config: &ServerConfig) -> Result<Value, ApprovalsError> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| ApprovalsError::Transport(e.to_string()))?;
    let response = client
        .get(format!("{}/approvals", config.base_url))
        .bearer_auth(&config.admin_token)
        .send()
        .await
        .map_err(|e| ApprovalsError::Transport(e.to_string()))?;
    handle_response(response).await
}

/// Approve or reject one pending card by id.
///
/// # Errors
/// Returns [`ApprovalsError::InvalidId`] without making any network call if
/// `id` fails the local validity check; otherwise see [`ApprovalsError`].
pub async fn decide(
    config: &ServerConfig,
    id: &str,
    decision: Decision,
) -> Result<Value, ApprovalsError> {
    if !valid_approval_id(id) {
        return Err(ApprovalsError::InvalidId);
    }

    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| ApprovalsError::Transport(e.to_string()))?;
    let (verb, body) = match decision {
        Decision::Approve => ("approve", None),
        Decision::Reject { reason } => ("reject", Some(serde_json::json!({ "reason": reason }))),
    };
    let mut request = client
        .post(format!("{}/approvals/{id}/{verb}", config.base_url))
        .bearer_auth(&config.admin_token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|e| ApprovalsError::Transport(e.to_string()))?;
    handle_response(response).await
}

/// Translate an `ardur-server` response into a typed result: `2xx` decodes
/// the JSON body, `401` maps to [`ApprovalsError::Unauthorized`], anything
/// else carries the status and (best-effort) body.
async fn handle_response(response: reqwest::Response) -> Result<Value, ApprovalsError> {
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ApprovalsError::Unauthorized);
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| ApprovalsError::InvalidResponse {
            status: status.as_u16(),
        })?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(ApprovalsError::ServerError {
            status: status.as_u16(),
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_ids_accepted() {
        assert!(valid_approval_id("abc123"));
        assert!(valid_approval_id("a-b_c"));
    }

    #[test]
    fn empty_id_rejected() {
        assert!(!valid_approval_id(""));
    }

    #[test]
    fn path_traversal_shaped_ids_rejected() {
        assert!(!valid_approval_id(".."));
        assert!(!valid_approval_id("a/b"));
        assert!(!valid_approval_id("../../etc/passwd"));
        assert!(!valid_approval_id("a.json"));
    }

    #[test]
    fn oversized_id_rejected() {
        let long = "a".repeat(MAX_APPROVAL_ID_LEN + 1);
        assert!(!valid_approval_id(&long));
    }

    #[test]
    fn max_length_id_accepted() {
        let max = "a".repeat(MAX_APPROVAL_ID_LEN);
        assert!(valid_approval_id(&max));
    }

    #[tokio::test]
    async fn decide_rejects_invalid_id_without_a_network_call() {
        // No mock server configured at all — if this made a network call it
        // would fail to connect and return a Transport error, not InvalidId.
        let config = ServerConfig::new("http://127.0.0.1:1", "token");
        let result = decide(&config, "../escape", Decision::Approve).await;
        assert!(matches!(result, Err(ApprovalsError::InvalidId)));
    }
}
