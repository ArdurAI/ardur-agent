//! The HTTP surface: an axum [`Router`] over [`AppState`].
//!
//! - `POST /slack/events` — verify + parse a Slack Events-API request, then
//!   enqueue a genuine user message for the worker (which runs the fused turn
//!   and posts the reply). Always answered with `200` for a verified request, so
//!   Slack does not retry.
//! - `POST /chat` — a generic, synchronous chat surface: run one turn through the
//!   fused runtime and return the consolidated result (reply, tokens, cost, tools
//!   called, receipt id) as JSON. Unlike Slack, the caller receives the reply
//!   directly rather than having it posted to a channel.
//! - `GET /healthz` — a legacy liveness probe carrying build metadata.
//! - `GET /health` — readiness with dependency checks.
//! - `GET /metrics` — Prometheus-compatible, secret-free process metrics.
//! - `GET /admin/runtime` — bearer-gated runtime inspection snapshot.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::json;

use ardur_acp::{
    ACP_METHOD_INITIALIZE, ACP_METHOD_SESSION_PROMPT, AcpErrorObject, AcpMessage, AcpRequest,
    AcpResponse,
};
use ardur_runtime::SessionId;
use ardur_slack_adapter::{SlackError, SlackEvent, SlackHeaders};

use crate::openapi::{generate_python_client, generate_rust_client, openapi_spec};
use crate::state::{
    AUDIENCE, AppState, CAP_TTL_SECS, ChatSubmitError, ChatTurnOutcome, GATEWAY_SUBJECT,
};

/// Build the application router over the shared [`AppState`].
///
/// Always mounts `POST /slack/events`, `POST /chat`, and `GET /healthz`. When the
/// MCP surface is enabled (see [`AppState::mcp`]), the bearer-gated MCP routes are
/// merged in at the configured path prefix.
pub fn build_router(state: Arc<AppState>) -> Router {
    let mut router = Router::new()
        .route("/slack/events", post(slack_events))
        .route("/chat", post(chat))
        .route("/acp", post(acp))
        .route("/healthz", get(healthz))
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/admin/runtime", get(admin_runtime))
        .route("/openapi.json", get(openapi_json))
        .route("/openapi/clients/rust", get(openapi_rust_client))
        .route("/openapi/clients/python", get(openapi_python_client));

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

/// `GET /health` — readiness probe with dependency checks.
async fn health(State(state): State<Arc<AppState>>) -> Response {
    let data_dir_ok = state.data_dir().is_dir();
    let journal_ok = state.data_dir().join("journals").is_dir();
    let worker_ok = state.worker_alive();
    let status = if data_dir_ok && journal_ok && worker_ok {
        "ok"
    } else {
        "degraded"
    };
    let code = if status == "ok" {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        code,
        Json(json!({
            "status": status,
            "dependencies": {
                "data_dir": if data_dir_ok { "ok" } else { "error" },
                "journal": if journal_ok { "ok" } else { "error" },
                "worker": if worker_ok { "ok" } else { "error" },
            },
        })),
    )
        .into_response()
}

/// `GET /metrics` — Prometheus text exposition with counts only (no tokens or
/// credentials). Keep labels low-cardinality and redact-by-design.
async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    let version = prometheus_label_value(env!("CARGO_PKG_VERSION"));
    let mut body = String::new();
    let _ = writeln!(body, "# HELP ardur_server_build_info Build metadata.");
    let _ = writeln!(body, "# TYPE ardur_server_build_info gauge");
    let _ = writeln!(body, "ardur_server_build_info{{version=\"{version}\"}} 1");
    let _ = writeln!(
        body,
        "# HELP ardur_server_receipts_total Persisted receipt count."
    );
    let _ = writeln!(body, "# TYPE ardur_server_receipts_total counter");
    let _ = writeln!(
        body,
        "ardur_server_receipts_total {}",
        state.receipt_count()
    );
    let _ = writeln!(
        body,
        "# HELP ardur_server_worker_alive Whether the turn worker is accepting work."
    );
    let _ = writeln!(body, "# TYPE ardur_server_worker_alive gauge");
    let _ = writeln!(
        body,
        "ardur_server_worker_alive {}",
        u8::from(state.worker_alive())
    );
    let _ = writeln!(
        body,
        "# HELP ardur_server_admin_bearer_tokens_configured Configured admin bearer token count (values redacted)."
    );
    let _ = writeln!(
        body,
        "# TYPE ardur_server_admin_bearer_tokens_configured gauge"
    );
    let _ = writeln!(
        body,
        "ardur_server_admin_bearer_tokens_configured {}",
        state.admin_bearer_tokens().len()
    );

    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// `GET /openapi.json` — return the generated OpenAPI 3.0 document.
async fn openapi_json() -> Response {
    Json(openapi_spec()).into_response()
}

/// `GET /openapi/clients/rust` — return generated Rust client source.
async fn openapi_rust_client() -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        generate_rust_client(),
    )
        .into_response()
}

/// `GET /admin/runtime` — a bearer-gated, redacted snapshot of runtime security
/// posture and receipt/gate counters. Missing admin tokens fail closed.
async fn admin_runtime(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_admin(&state, &headers) {
        return *response;
    }

    Json(json!({
        "cap_tokens": {
            "audience": AUDIENCE,
            "gateway_subject": GATEWAY_SUBJECT,
            "ttl_secs": CAP_TTL_SECS,
            "tool_allowlist_count": state.tool_allowlist().len(),
        },
        "receipts": {
            "count": state.receipt_count(),
        },
        "gates": {
            "cost_budget_cents": state.cost_budget_cents(),
        },
        "tools": {
            "allowlist_count": state.tool_allowlist().len(),
        },
        "surfaces": {
            "mcp_enabled": state.mcp().is_some(),
            "chat_auth_required": !state.chat_bearer_tokens().is_empty(),
            "admin_auth_required": true,
        },
    }))
    .into_response()
}

fn prometheus_label_value(raw: &str) -> String {
    raw.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// `GET /openapi/clients/python` — return generated Python client source.
async fn openapi_python_client() -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        generate_python_client(),
    )
        .into_response()
}

/// The `POST /chat` request body.
///
/// `session_id` is a UUID string (the runtime's [`SessionId`]); omit it to have
/// the server mint a fresh one. `stream` requests an SSE response — not yet
/// implemented (P1.5), so a `true` value is rejected with `400`.
#[derive(Debug, Deserialize)]
struct ChatRequest {
    /// The user's message to run one turn against. Required and non-empty.
    message: String,
    /// The session this turn belongs to; minted fresh when absent.
    #[serde(default)]
    session_id: Option<SessionId>,
    /// Whether to stream the reply as SSE. Unsupported in P1 (see the handler).
    #[serde(default)]
    stream: bool,
}

/// The `POST /chat` success response body.
#[derive(Debug, Serialize)]
struct ChatResponse {
    /// The session the turn ran under (echoed; minted when the request omitted it).
    session_id: SessionId,
    /// The assistant's reply text.
    reply: String,
    /// Token accounting for the turn.
    tokens: Tokens,
    /// Monetary cost of the turn, in US dollars.
    cost_usd: f64,
    /// The tools the model invoked over the turn, in receipt order.
    tools_called: Vec<String>,
    /// The id of the (final) receipt minted for the turn.
    receipt_id: String,
}

/// Prompt/completion token counts, as the response's `tokens` object.
#[derive(Debug, Serialize)]
struct Tokens {
    /// Prompt/input tokens billed.
    input: u64,
    /// Completion/output tokens billed.
    output: u64,
}

impl From<ChatTurnOutcome> for ChatResponse {
    fn from(outcome: ChatTurnOutcome) -> Self {
        ChatResponse {
            session_id: outcome.session_id,
            reply: outcome.reply,
            tokens: Tokens {
                input: outcome.tokens_in,
                output: outcome.tokens_out,
            },
            // Receipts account cost in whole US cents; the HTTP surface reports
            // dollars for an embedding client's convenience.
            cost_usd: outcome.cents as f64 / 100.0,
            tools_called: outcome.tools_called,
            receipt_id: outcome.receipt_id,
        }
    }
}

/// `POST /chat` — the generic synchronous chat entry point.
///
/// Parses the JSON body, runs one turn through the fused runtime via
/// [`AppState::submit_chat`], and returns the consolidated result. Status codes:
/// `400` for a malformed body, a missing/empty `message`, or `stream: true`
/// (unsupported); `502` when the runtime rejects or fails the turn (cost gate
/// denied, injection blocked, provider error, …); `200` otherwise.
async fn chat(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = authorize_chat(&state, &headers) {
        return *response;
    }

    if body.len() > 64 * 1024 {
        return bad_request("request body exceeds 64KiB".to_string());
    }

    let request: ChatRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => return bad_request(format!("invalid request body: {e}")),
    };

    if request.message.trim().is_empty() {
        return bad_request("`message` is required and must be non-empty".to_string());
    }

    // Streaming (SSE) is the P1.5 follow-up — fail loudly rather than silently
    // returning a non-streamed body the caller did not ask for.
    if request.stream {
        return bad_request(
            "`stream: true` is not yet supported; omit it for a consolidated reply".to_string(),
        );
    }

    // A provided session id threads a follow-up onto the same session; an absent
    // one mints a fresh, time-ordered session (`SessionId::default` == `new`).
    let session_id = request.session_id.unwrap_or_default();

    match state.submit_chat(request.message, session_id).await {
        Ok(outcome) => (StatusCode::OK, Json(ChatResponse::from(outcome))).into_response(),
        // The runtime rejected or failed the turn — a bad-gateway from the HTTP
        // surface's point of view (the upstream pipeline refused or errored).
        Err(ChatSubmitError::Runtime(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(ChatSubmitError::WorkerGone) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "turn worker is unavailable" })),
        )
            .into_response(),
    }
}

/// `POST /acp` — accept one ACP JSON-RPC envelope over HTTP.
///
/// This endpoint deliberately reuses the same bearer gate and fused-runtime
/// submission path as `/chat`: the server mints a scoped cap-token, Cedar checks
/// `Action::Submit`, the cost gate admits the work, and a signed receipt is
/// appended to the chain before the JSON-RPC response is returned.
async fn acp(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = authorize_chat(&state, &headers) {
        return *response;
    }
    if body.len() > 64 * 1024 {
        return bad_request("request body exceeds 64KiB".to_string());
    }
    let message: AcpMessage = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(e) => return bad_request(format!("invalid ACP message: {e}")),
    };
    if let Err(e) = message.validate() {
        return bad_request(format!("invalid ACP message: {e}"));
    }
    let AcpMessage::Request(request) = message else {
        return bad_request("ACP HTTP ingress accepts requests only".to_string());
    };

    let prompt = match acp_prompt_for_request(&request) {
        Ok(prompt) => prompt,
        Err(error) => {
            return (
                StatusCode::OK,
                Json(AcpMessage::Response(AcpResponse::failure(
                    request.id, error,
                ))),
            )
                .into_response();
        }
    };
    match state.submit_chat(prompt, SessionId::new()).await {
        Ok(outcome) => {
            let result = json!({
                "accepted": true,
                "method": request.method,
                "receipt_id": outcome.receipt_id,
                "reply": outcome.reply,
                "tokens": {
                    "input": outcome.tokens_in,
                    "output": outcome.tokens_out,
                },
                "cost_usd": outcome.cents as f64 / 100.0,
            });
            (
                StatusCode::OK,
                Json(AcpMessage::Response(AcpResponse::success(
                    request.id, result,
                ))),
            )
                .into_response()
        }
        Err(ChatSubmitError::Runtime(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(AcpMessage::Response(AcpResponse::failure(
                request.id,
                AcpErrorObject::new(-32000, e.to_string(), None),
            ))),
        )
            .into_response(),
        Err(ChatSubmitError::WorkerGone) => (
            StatusCode::BAD_GATEWAY,
            Json(AcpMessage::Response(AcpResponse::failure(
                request.id,
                AcpErrorObject::new(-32001, "turn worker is unavailable", None),
            ))),
        )
            .into_response(),
    }
}

fn acp_prompt_for_request(request: &AcpRequest) -> Result<String, AcpErrorObject> {
    match request.method.as_str() {
        ACP_METHOD_INITIALIZE => {
            if let Some(params) = &request.params {
                let protocol = params
                    .get("protocolVersion")
                    .or_else(|| params.get("protocol_version"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1);
                if protocol != 1 {
                    return Err(AcpErrorObject::new(
                        -32602,
                        format!("unsupported ACP protocol version {protocol}"),
                        None,
                    ));
                }
            }
            Ok("ACP initialize request accepted for protocol v1".to_string())
        }
        ACP_METHOD_SESSION_PROMPT => {
            let params = request
                .params
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| {
                    AcpErrorObject::new(-32602, "session/prompt params must be an object", None)
                })?;
            let message = params
                .get("prompt")
                .or_else(|| params.get("message"))
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    AcpErrorObject::new(
                        -32602,
                        "session/prompt requires a non-empty prompt or message string",
                        None,
                    )
                })?;
            Ok(format!("ACP session prompt: {message}"))
        }
        other => Err(AcpErrorObject::new(
            -32601,
            format!("unsupported ACP method `{other}`"),
            None,
        )),
    }
}

/// Verify the `POST /chat` bearer token before body processing or provider work.
fn authorize_chat(state: &AppState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    authorize_bearer(state.chat_bearer_tokens(), headers).map_err(|()| Box::new(unauthorized()))
}

/// Verify the admin bearer token. Empty admin token config fails closed.
fn authorize_admin(state: &AppState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    authorize_bearer(state.admin_bearer_tokens(), headers).map_err(|()| Box::new(unauthorized()))
}

fn authorize_bearer(allowed_tokens: &[String], headers: &HeaderMap) -> Result<(), ()> {
    let presented = header_str(headers, "Authorization");
    let Some(token) = presented.strip_prefix("Bearer ") else {
        return Err(());
    };
    if !allowed_tokens.is_empty() && allowed_tokens.iter().any(|allowed| allowed == token) {
        Ok(())
    } else {
        Err(())
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "missing or invalid bearer token" })),
    )
        .into_response()
}

/// A `400` with a JSON `{ "error": … }` body.
fn bad_request(message: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
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
