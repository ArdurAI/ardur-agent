//! Optional HTTP Basic and/or Bearer auth — a gate for a shared network.
//!
//! Two independent, optional mechanisms. Either may be configured alone, both
//! together (a request satisfying *either* passes), or neither (the default —
//! no gate, matching the original trust model of a local/private network).
//! Bearer is the preferred mechanism for anything reachable beyond loopback:
//! it mirrors `ardur-server`'s `authorize_admin` fail-closed multi-token
//! bearer check (`crates/server/src/routes.rs`) — bounded token length,
//! constant-time comparison against every configured token, fail-closed when
//! configured but the presented token matches none of them.

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use subtle::ConstantTimeEq;

use crate::state::SharedState;

/// Reject bearer tokens longer than this before any comparison work, so a
/// pre-auth request cannot force unbounded memory/CPU churn.
const MAX_BEARER_TOKEN_LEN: usize = 4096;

/// Configured Basic credentials, pre-rendered into the exact `Authorization`
/// header value a matching request must present.
#[derive(Clone)]
pub struct BasicAuth {
    expected_header: String,
}

impl std::fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak the credential into logs / Debug output.
        f.debug_struct("BasicAuth").finish_non_exhaustive()
    }
}

impl BasicAuth {
    /// Build from a `user:pass` string. The colon separates the two; a password
    /// may itself contain colons.
    #[must_use]
    pub fn from_user_pass(user_pass: &str) -> Self {
        let expected_header = format!("Basic {}", B64.encode(user_pass.as_bytes()));
        Self { expected_header }
    }

    /// Whether a presented `Authorization` header value matches.
    fn matches(&self, header_value: &str) -> bool {
        // Constant-time comparison via `subtle` — prevents timing side-channel
        // attacks that could leak the credential byte-by-byte.
        header_value
            .as_bytes()
            .ct_eq(self.expected_header.as_bytes())
            .into()
    }
}

/// One or more configured bearer tokens. A request is authorized if its
/// `Authorization: Bearer <token>` matches any of them.
#[derive(Clone)]
pub struct BearerAuth {
    tokens: Vec<String>,
}

impl std::fmt::Debug for BearerAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak token values into logs / Debug output.
        f.debug_struct("BearerAuth")
            .field("token_count", &self.tokens.len())
            .finish()
    }
}

impl BearerAuth {
    /// Build from a list of accepted tokens. Empty tokens are dropped — an
    /// empty string must never match, since a request with no `Authorization`
    /// header at all is otherwise represented the same way.
    #[must_use]
    pub fn from_tokens(tokens: Vec<String>) -> Self {
        Self {
            tokens: tokens.into_iter().filter(|t| !t.is_empty()).collect(),
        }
    }

    /// Whether a presented bearer token (the part after `Bearer `) matches any
    /// configured token. Fails closed if no tokens are configured.
    fn matches(&self, presented: &str) -> bool {
        if presented.len() > MAX_BEARER_TOKEN_LEN || self.tokens.is_empty() {
            return false;
        }
        let presented_bytes = presented.as_bytes();
        let presented_padded = padded_bearer::<MAX_BEARER_TOKEN_LEN>(presented_bytes);
        let presented_len = presented_bytes.len() as u64;
        let mut found = subtle::Choice::from(0);
        for allowed in &self.tokens {
            let allowed_bytes = allowed.as_bytes();
            let allowed_padded = padded_bearer::<MAX_BEARER_TOKEN_LEN>(allowed_bytes);
            let allowed_len = allowed_bytes.len() as u64;
            found |= presented_len.ct_eq(&allowed_len)
                & presented_padded.as_slice().ct_eq(allowed_padded.as_slice());
        }
        found.into()
    }
}

fn padded_bearer<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut out = [0_u8; N];
    for (idx, slot) in out.iter_mut().enumerate() {
        *slot = bytes.get(idx).copied().unwrap_or(0);
    }
    out
}

/// Axum middleware enforcing Basic and/or Bearer auth per whichever of
/// [`AppState::basic_auth`] / [`AppState::bearer_auth`] is set. When neither
/// is set the request passes straight through. When at least one is set, the
/// request is authorized if it satisfies *any* configured mechanism.
///
/// [`AppState::basic_auth`]: crate::state::AppState::basic_auth
/// [`AppState::bearer_auth`]: crate::state::AppState::bearer_auth
pub async fn require_auth(
    State(state): State<SharedState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if state.basic_auth.is_none() && state.bearer_auth.is_none() {
        return next.run(request).await;
    }

    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let basic_ok = state
        .basic_auth
        .as_ref()
        .zip(presented)
        .is_some_and(|(auth, header)| auth.matches(header));
    let bearer_ok = state
        .bearer_auth
        .as_ref()
        .zip(presented.and_then(|h| h.strip_prefix("Bearer ")))
        .is_some_and(|(auth, token)| auth.matches(token));

    if basic_ok || bearer_ok {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"ardur-admin\"")],
            "unauthorized",
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_matches_configured_token() {
        let auth = BearerAuth::from_tokens(vec!["secret-token".to_string()]);
        assert!(auth.matches("secret-token"));
    }

    #[test]
    fn bearer_rejects_wrong_token() {
        let auth = BearerAuth::from_tokens(vec!["secret-token".to_string()]);
        assert!(!auth.matches("wrong-token"));
    }

    #[test]
    fn bearer_matches_any_of_multiple_tokens() {
        let auth = BearerAuth::from_tokens(vec!["one".to_string(), "two".to_string()]);
        assert!(auth.matches("one"));
        assert!(auth.matches("two"));
        assert!(!auth.matches("three"));
    }

    #[test]
    fn bearer_fails_closed_when_unconfigured() {
        let auth = BearerAuth::from_tokens(Vec::new());
        assert!(!auth.matches("anything"));
        // An empty presented token must not match an (incorrectly) empty
        // configured token either.
        assert!(!auth.matches(""));
    }

    #[test]
    fn bearer_drops_empty_configured_tokens() {
        let auth = BearerAuth::from_tokens(vec![String::new(), "real".to_string()]);
        assert!(!auth.matches(""));
        assert!(auth.matches("real"));
    }

    #[test]
    fn bearer_rejects_oversized_token_without_panicking() {
        let auth = BearerAuth::from_tokens(vec!["short".to_string()]);
        let huge = "a".repeat(MAX_BEARER_TOKEN_LEN + 1);
        assert!(!auth.matches(&huge));
    }
}
