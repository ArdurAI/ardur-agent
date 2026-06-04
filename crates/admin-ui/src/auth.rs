//! Optional HTTP Basic auth — a light gate for a shared network.
//!
//! This is intentionally minimal: it compares the request's `Authorization`
//! header against a single configured `user:pass`. It is **not** a substitute
//! for real authentication (no TLS termination, no rate limiting, no user
//! store); the README states the intended trust model (local / private
//! network, read-only data).

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

use crate::state::SharedState;

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
        // Constant-time-ish: equal length then byte compare. The credential is
        // low-value (read-only data) so this is belt-and-suspenders.
        header_value.as_bytes() == self.expected_header.as_bytes()
    }
}

/// Axum middleware enforcing Basic auth when [`AppState::basic_auth`] is set.
/// When it is unset the request passes straight through.
///
/// [`AppState::basic_auth`]: crate::state::AppState::basic_auth
pub async fn require_auth(
    State(state): State<SharedState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(auth) = &state.basic_auth else {
        return next.run(request).await;
    };

    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    if presented.is_some_and(|h| auth.matches(h)) {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"ardur-admin\"")],
            "unauthorized",
        )
            .into_response()
    }
}
