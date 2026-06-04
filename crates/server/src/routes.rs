//! The HTTP surface: a two-route axum [`Router`] over [`AppState`].
//!
//! - `POST /slack/events` — verify + parse a Slack Events-API request, then
//!   enqueue a genuine user message for the worker (which runs the fused turn
//!   and posts the reply). Always answered with `200` for a verified request, so
//!   Slack does not retry.
//! - `GET /healthz` — a liveness probe carrying build metadata.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde_json::json;

use ardur_slack_adapter::{SlackError, SlackEvent, SlackHeaders};

use crate::state::AppState;

/// Build the application router over the shared [`AppState`].
///
/// Always mounts `POST /slack/events` and `GET /healthz`. When the MCP surface
/// is enabled (see [`AppState::mcp`]), the bearer-gated MCP routes are merged in
/// at the configured path prefix.
pub fn build_router(state: Arc<AppState>) -> Router {
    let mut router = Router::new()
        .route("/slack/events", post(slack_events))
        .route("/healthz", get(healthz));

    if let Some(mcp) = state.mcp() {
        router = router.merge(crate::build_mcp_router(
            mcp.registry.clone(),
            mcp.bearer_tokens.clone(),
            &mcp.path_prefix,
        ));
    }

    router.with_state(state)
}

/// `GET /healthz` — always 200 with build metadata.
async fn healthz() -> Response {
    Json(json!({
        "status": "ok",
        "build": env!("CARGO_PKG_VERSION"),
        "tests": "147",
    }))
    .into_response()
}

/// `POST /slack/events` — the Slack webhook entry point.
///
/// Reads the raw body *before* deserializing (the HMAC is over the exact bytes),
/// hands it to the adapter (which fails closed on a bad signature or replay),
/// and dispatches on the parsed [`SlackEvent`]. A genuine message is enqueued
/// for the worker and acknowledged with `200` — Slack retries non-2xx, and a
/// failed turn is reported into the channel rather than by HTTP status.
async fn slack_events(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let slack_headers = SlackHeaders::new(
        header_str(&headers, "X-Slack-Signature"),
        header_str(&headers, "X-Slack-Request-Timestamp"),
    );

    // The signature is over the exact request bytes — verify against them, and
    // reject a body that is not even valid UTF-8 (Slack always sends UTF-8 JSON).
    let Ok(body_str) = std::str::from_utf8(&body) else {
        return (StatusCode::BAD_REQUEST, "request body is not valid UTF-8").into_response();
    };

    match state.slack().parse_event(&slack_headers, body_str) {
        Ok(SlackEvent::UrlVerification { challenge }) => {
            (StatusCode::OK, Json(json!({ "challenge": challenge }))).into_response()
        }
        // A verified event we deliberately drop (e.g. our own bot's message).
        Ok(SlackEvent::Ignored) => StatusCode::OK.into_response(),
        Ok(SlackEvent::Message(incoming)) => {
            if !state.enqueue(incoming) {
                tracing::error!("turn worker is gone; dropping message");
            }
            StatusCode::OK.into_response()
        }
        // Fail closed on a forged signature or a replayed (stale/future) request.
        Err(SlackError::InvalidSignature) | Err(SlackError::Replay { .. }) => {
            (StatusCode::UNAUTHORIZED, "signature verification failed").into_response()
        }
        // A verified-but-unsupported event type (e.g. a reaction): acknowledge so
        // Slack does not retry.
        Err(SlackError::UnsupportedEvent(kind)) => {
            tracing::debug!(event = %kind, "ignoring unsupported slack event");
            StatusCode::OK.into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "rejecting malformed slack request");
            (StatusCode::BAD_REQUEST, "bad request").into_response()
        }
    }
}

/// A header value as a `&str`, or empty when absent / non-ASCII (the adapter
/// then rejects it on the signature check).
fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}
