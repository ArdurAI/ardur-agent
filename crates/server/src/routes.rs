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

use std::convert::Infallible;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::json;
use subtle::ConstantTimeEq;

use ardur_acp::{
    ACP_METHOD_INITIALIZE, ACP_METHOD_SESSION_PROMPT, AcpErrorObject, AcpMessage, AcpRequest,
    AcpResponse,
};
use ardur_fused_runtime::{FusedEvent, StageKind};
use ardur_runtime::{RuntimeError, SessionId};
use ardur_session_journals::JournalEntry;
use ardur_slack_adapter::{SlackError, SlackEvent, SlackHeaders};

use crate::openapi::{generate_python_client, generate_rust_client, openapi_spec};
use crate::state::{
    AUDIENCE, AppState, CAP_TTL_SECS, ChatSubmitError, ChatTurnOutcome, GATEWAY_SUBJECT,
};

const HTTP_BODY_LIMIT_BYTES: usize = 64 * 1024;
const HTTP_TURN_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the application router over the shared [`AppState`].
///
/// Always mounts `POST /chat` and `GET /healthz`. `POST /slack/events` is mounted
/// only when the Slack channel is enabled (see [`AppState::slack`]) — an HTTP-only
/// boot omits it, so an inbound Slack request there gets a `404`. When the MCP
/// surface is enabled (see [`AppState::mcp`]), the bearer-gated MCP routes are
/// merged in at the configured path prefix.
pub fn build_router(state: Arc<AppState>) -> Router {
    let mut router = Router::new()
        .route("/chat", post(chat))
        .route("/acp", post(acp))
        .route("/healthz", get(healthz))
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/admin/runtime", get(admin_runtime))
        .route("/approvals", get(approvals_list))
        .route("/approvals/{id}/approve", post(approvals_approve))
        .route("/approvals/{id}/reject", post(approvals_reject))
        .route("/openapi.json", get(openapi_json))
        .route("/openapi/clients/rust", get(openapi_rust_client))
        .route("/openapi/clients/python", get(openapi_python_client));

    // The Slack webhook is mounted only when Slack is enabled, mirroring the
    // conditional MCP merge below. HTTP-only boots leave it off entirely.
    if state.slack().is_some() {
        router = router.route("/slack/events", post(slack_events));
    }

    if let Some(mcp) = state.mcp() {
        router = router.merge(crate::build_mcp_router(
            mcp.registry.clone(),
            mcp.bearer_tokens.clone(),
            &mcp.path_prefix,
        ));
    }

    router
        .layer(DefaultBodyLimit::max(HTTP_BODY_LIMIT_BYTES))
        .with_state(state)
}

/// `GET /healthz` — always 200 with build metadata.
async fn healthz() -> Response {
    Json(json!({
        "status": "ok",
        "build": env!("CARGO_PKG_VERSION"),
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
///
/// Bearer-gated via the admin token set: metrics expose operational counts
/// (receipt totals, budget, build version) that could aid reconnaissance.
/// When no admin tokens are configured the endpoint fails closed (401).
async fn metrics(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_admin(&state, &headers) {
        return *response;
    }

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

    // Receipt-chain aggregates, rolled up from disk at scrape time. Counts and
    // summed cost only — no message content, no per-principal identifiers.
    let stats = state.receipt_stats();
    let _ = writeln!(
        body,
        "# HELP ardur_server_receipt_chain_verified Whether the persisted receipt chain verified (hash + signatures)."
    );
    let _ = writeln!(body, "# TYPE ardur_server_receipt_chain_verified gauge");
    let _ = writeln!(
        body,
        "ardur_server_receipt_chain_verified {}",
        u8::from(stats.chain_verified)
    );
    let _ = writeln!(
        body,
        "# HELP ardur_server_receipt_cost_cents_total Summed settled cost across all receipts, in cents."
    );
    let _ = writeln!(body, "# TYPE ardur_server_receipt_cost_cents_total counter");
    let _ = writeln!(
        body,
        "ardur_server_receipt_cost_cents_total {}",
        stats.cost_cents_sum
    );
    let _ = writeln!(
        body,
        "# HELP ardur_server_tool_calls_total Tool calls attested across the receipt chain."
    );
    let _ = writeln!(body, "# TYPE ardur_server_tool_calls_total counter");
    let _ = writeln!(
        body,
        "ardur_server_tool_calls_total {}",
        stats.tool_calls_sum
    );
    let _ = writeln!(
        body,
        "# HELP ardur_server_sessions_total Distinct sessions spanned by the receipt chain."
    );
    let _ = writeln!(body, "# TYPE ardur_server_sessions_total gauge");
    let _ = writeln!(
        body,
        "ardur_server_sessions_total {}",
        stats.distinct_sessions
    );
    let _ = writeln!(
        body,
        "# HELP ardur_server_receipts_by_verb Receipt count keyed by verb."
    );
    let _ = writeln!(body, "# TYPE ardur_server_receipts_by_verb counter");
    for (verb, count) in &stats.by_verb {
        let _ = writeln!(
            body,
            "ardur_server_receipts_by_verb{{verb=\"{}\"}} {count}",
            prometheus_label_value(verb)
        );
    }
    let _ = writeln!(
        body,
        "# HELP ardur_server_receipts_by_provider Receipt count keyed by model backend."
    );
    let _ = writeln!(body, "# TYPE ardur_server_receipts_by_provider counter");
    for (provider, count) in &stats.by_provider {
        let _ = writeln!(
            body,
            "ardur_server_receipts_by_provider{{provider=\"{}\"}} {count}",
            prometheus_label_value(provider)
        );
    }

    // Turn-outcome and security-denial counters, accumulated by the worker over
    // the process lifetime. Counts only — the *why* of a block lives in tracing.
    let sec = state.security_metrics().snapshot();
    let _ = writeln!(
        body,
        "# HELP ardur_server_turns_ok_total Turns that settled with a minted receipt."
    );
    let _ = writeln!(body, "# TYPE ardur_server_turns_ok_total counter");
    let _ = writeln!(body, "ardur_server_turns_ok_total {}", sec.turns_ok);
    let _ = writeln!(
        body,
        "# HELP ardur_server_turns_denied_total Turns blocked by a security gate, keyed by gate."
    );
    let _ = writeln!(body, "# TYPE ardur_server_turns_denied_total counter");
    for (gate, count) in [
        ("injection", sec.injection_blocked),
        ("policy", sec.policy_denied),
        ("cap_token", sec.cap_denied),
        ("cost", sec.cost_rejected),
        ("hook", sec.hook_vetoed),
        ("tool", sec.tool_denied),
    ] {
        let _ = writeln!(
            body,
            "ardur_server_turns_denied_total{{gate=\"{gate}\"}} {count}"
        );
    }
    let _ = writeln!(
        body,
        "# HELP ardur_server_turns_errored_total Turns that failed for non-security reasons (provider/internal)."
    );
    let _ = writeln!(body, "# TYPE ardur_server_turns_errored_total counter");
    let _ = writeln!(
        body,
        "ardur_server_turns_errored_total {}",
        sec.other_errors
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
///
/// Bearer-gated via the admin token set: the schema exposes the full API
/// surface (request/response shapes) which aids reconnaissance. When no
/// admin tokens are configured the endpoint fails closed (401).
async fn openapi_json(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_admin(&state, &headers) {
        return *response;
    }
    Json(openapi_spec(state.slack().is_some())).into_response()
}

/// `GET /openapi/clients/rust` — return generated Rust client source.
///
/// Same admin bearer gate as the OpenAPI schema.
async fn openapi_rust_client(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_admin(&state, &headers) {
        return *response;
    }
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

    let stats = state.receipt_stats();
    let sec = state.security_metrics().snapshot();
    Json(json!({
        "cap_tokens": {
            "audience": AUDIENCE,
            "gateway_subject": GATEWAY_SUBJECT,
            "ttl_secs": CAP_TTL_SECS,
            "tool_allowlist_count": state.tool_allowlist().len(),
        },
        "receipts": {
            "count": state.receipt_count(),
            "chain_verified": stats.chain_verified,
            "cost_cents_total": stats.cost_cents_sum,
            "tool_calls_total": stats.tool_calls_sum,
            "distinct_sessions": stats.distinct_sessions,
        },
        "gates": {
            "cost_budget_cents": state.cost_budget_cents(),
        },
        "turns": {
            "ok": sec.turns_ok,
            "errored": sec.other_errors,
            "denied": {
                "injection": sec.injection_blocked,
                "policy": sec.policy_denied,
                "cap_token": sec.cap_denied,
                "cost": sec.cost_rejected,
                "hook": sec.hook_vetoed,
                "tool": sec.tool_denied,
            },
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

// ---------------------------------------------------------------------------
// Approval-gate decide endpoints (ARD-139 decide-half)
//
// These are the *decide* side of the approval loop: they let an admin (the
// `web-client/` PWA, or any bearer-holding operator) approve or reject a pending
// approval card and list outstanding cards. They are backed by the exact same
// on-disk store the CLI's `ardur approvals` subcommand uses
// (`<data_dir>/approvals/<id>.json`), so the HTTP surface and the CLI share a
// single source of truth. Nothing server-side currently *produces* pending
// cards — that is the propose-half, a separate follow-up.
// ---------------------------------------------------------------------------

/// Cap on the accepted approval-id length. Ids are opaque filenames-stems; a
/// generous-but-bounded ceiling keeps a hostile path from ballooning a `PathBuf`.
const MAX_APPROVAL_ID_LEN: usize = 128;

/// Whether `id` is a safe approval identifier: non-empty, bounded, and drawn
/// only from `[A-Za-z0-9_-]`. This is the traversal guard — because `.` and `/`
/// are rejected, no `id` can escape the approvals directory (`..`, `a/b`,
/// absolute paths, or NUL bytes are all refused) before it is ever joined onto a
/// path.
fn valid_approval_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_APPROVAL_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Current wall-clock seconds since the Unix epoch (matches the CLI store's
/// `decided_at` unit).
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current wall-clock milliseconds since the Unix epoch (the journal `at` unit).
fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The decision an operator is recording against a pending card.
enum ApprovalDecision {
    Approve,
    Reject { reason: String },
}

/// `GET /approvals` — list every approval card in the store as a JSON array.
///
/// Each element is the stored record with its `id` (the filename stem) injected,
/// so the response is self-describing. Admin-bearer gated; fails closed when no
/// admin tokens are configured.
async fn approvals_list(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_admin(&state, &headers) {
        return *response;
    }

    let dir = state.approvals_dir();
    let mut cards = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Only surface records whose id we would also accept on the decide
            // path — anything else is not a card this API manages.
            if !valid_approval_id(stem) {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(object) = value.as_object_mut() {
                        object.insert("id".to_string(), json!(stem));
                    }
                    cards.push(value);
                }
            }
        }
    }

    (StatusCode::OK, Json(json!(cards))).into_response()
}

/// `POST /approvals/{id}/approve` — flip a pending card to `approved`.
async fn approvals_approve(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authorize_admin(&state, &headers) {
        return *response;
    }
    apply_approval_decision(&state, &id, ApprovalDecision::Approve).await
}

/// `POST /approvals/{id}/reject` — flip a pending card to `denied`.
///
/// The wire verb is `reject` (what the PWA calls) but the stored status is
/// `denied`, reconciling with the CLI's vocabulary. An optional JSON body
/// `{ "reason": "…" }` is recorded as `deny_reason`; the PWA sends no body, so an
/// absent/empty/unparseable body simply yields an empty reason.
async fn approvals_reject(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = authorize_admin(&state, &headers) {
        return *response;
    }
    let reason = parse_reject_reason(&body);
    apply_approval_decision(&state, &id, ApprovalDecision::Reject { reason }).await
}

/// Extract an optional `reason` string from a reject body. Tolerant by design:
/// the PWA sends no body at all, so anything that is not a JSON object carrying a
/// string `reason` yields the empty reason rather than an error.
fn parse_reject_reason(body: &Bytes) -> String {
    if body.is_empty() {
        return String::new();
    }
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .as_ref()
        .and_then(|v| v.get("reason"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Shared decide path for approve/reject: validate the id, load the card, refuse
/// an already-decided card, mutate + durably persist, then best-effort audit.
///
/// Status codes: `400` for a malformed id, `404` for a missing card, `409` for a
/// card that is already decided (which keeps repeated calls idempotent-safe — the
/// mutation and the audit entry happen exactly once), `500` for a corrupt record
/// or a failed write, `200` on success (body is the updated record).
async fn apply_approval_decision(
    state: &Arc<AppState>,
    id: &str,
    decision: ApprovalDecision,
) -> Response {
    if !valid_approval_id(id) {
        return bad_request("malformed approval id".to_string());
    }

    let path = state.approvals_dir().join(format!("{id}.json"));
    if !path.is_file() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "approval not found" })),
        )
            .into_response();
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return internal_error("failed to read approval record"),
    };
    let mut card: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(card) => card,
        Err(_) => return internal_error("approval record is corrupt"),
    };

    let current_status = card
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("pending");
    if current_status != "pending" {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "approval already decided",
                "status": current_status,
            })),
        )
            .into_response();
    }

    let Some(object) = card.as_object_mut() else {
        return internal_error("approval record is not an object");
    };
    let (new_status, audit_verb, receipt_verb) = match &decision {
        ApprovalDecision::Approve => ("approved", "approved", "approval.approve.accepted.v1"),
        ApprovalDecision::Reject { reason } => {
            object.insert("deny_reason".to_string(), json!(reason));
            ("denied", "rejected", "approval.reject.accepted.v1")
        }
    };
    object.insert("status".to_string(), json!(new_status));
    object.insert("decided_at".to_string(), json!(now_unix_secs()));

    let serialized = match serde_json::to_string_pretty(&card) {
        Ok(serialized) => serialized,
        Err(_) => return internal_error("failed to serialize approval record"),
    };
    if let Err(e) = write_atomically(&path, serialized.as_bytes()) {
        tracing::error!(error = %e, approval_id = %id, "failed to persist approval decision");
        return internal_error("failed to persist approval decision");
    }

    // Audit trail: append the decision to the session journal. The decision is
    // already durable in the approvals store above (the source of truth), so a
    // journal failure is logged but does not fail the request.
    let audit = JournalEntry::Checkpoint {
        checkpoint_id: uuid::Uuid::now_v7(),
        summary: format!("approval {id} {audit_verb} via HTTP admin endpoint"),
        at: ardur_session_journals::UnixTsMillis(now_unix_millis()),
    };
    if let Err(e) = state.journal().append(audit).await {
        tracing::error!(error = %e, approval_id = %id, "failed to append approval audit entry");
    }

    // ARD-139: mint a signed receipt for the decision, chained onto the same
    // receipt log turns use. Like the journal audit above, the decision is
    // already durable in the approvals store — a minting failure (the turn
    // worker being unavailable, say) is logged but does not fail the request;
    // the store, not the receipt, is this endpoint's source of truth.
    match state
        .mint_approval_receipt(id.to_string(), receipt_verb.to_string())
        .await
    {
        Ok(receipt_id) => {
            if let Some(object) = card.as_object_mut() {
                object.insert("receipt_id".to_string(), json!(receipt_id.0.to_string()));
            }
        }
        Err(e) => {
            tracing::error!(error = %e, approval_id = %id, "failed to mint approval decision receipt");
        }
    }

    (StatusCode::OK, Json(card)).into_response()
}

/// Write `bytes` to `path` atomically: write a sibling temp file, fsync, then
/// rename over the target so a crash mid-write cannot leave a torn record.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let tmp = dir.join(format!(
        ".approval-{}.tmp",
        uuid::Uuid::now_v7().as_simple()
    ));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// A `500` with a JSON `{ "error": … }` body.
fn internal_error(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": message })),
    )
        .into_response()
}

/// `GET /openapi/clients/python` — return generated Python client source.
///
/// Same admin bearer gate as the OpenAPI schema.
async fn openapi_python_client(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_admin(&state, &headers) {
        return *response;
    }
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
/// the server mint a fresh one. `stream` requests the SSE response variant.
#[derive(Debug, Deserialize)]
struct ChatRequest {
    /// The user's message to run one turn against. Required and non-empty.
    message: String,
    /// The session this turn belongs to; minted fresh when absent.
    #[serde(default)]
    session_id: Option<SessionId>,
    /// Whether to stream the reply as SSE using progressive fused-runtime events.
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
/// `400` for a malformed/oversized body or missing/empty `message`; `502` when
/// the runtime rejects or fails the turn (cost gate denied, injection blocked,
/// provider error, …); `503` when the bounded worker queue is saturated; `504`
/// when the HTTP turn wait times out; `200` otherwise.
async fn chat(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(response) = authorize_chat(&state, &headers) {
        return *response;
    }

    if body.len() > HTTP_BODY_LIMIT_BYTES {
        return bad_request("request body exceeds 64KiB".to_string());
    }

    let request: ChatRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => return bad_request(format!("invalid request body: {e}")),
    };

    if request.message.trim().is_empty() {
        return bad_request("`message` is required and must be non-empty".to_string());
    }

    // Streaming (SSE) is the P1.5 follow-up — for now, we accept the flag and
    // return a mock SSE stream that yields the consolidated reply as a single
    // event. This unblocks the API contract while full streaming is implemented.
    if request.stream {
        return stream_chat(state, request.message, request.session_id).await;
    }

    // A provided session id threads a follow-up onto the same session; an absent
    // one mints a fresh, time-ordered session (`SessionId::default` == `new`).
    let session_id = request.session_id.unwrap_or_default();

    match tokio::time::timeout(
        HTTP_TURN_TIMEOUT,
        state.submit_chat(request.message, session_id),
    )
    .await
    {
        Err(_elapsed) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(json!({ "error": "turn processing timed out" })),
        )
            .into_response(),
        Ok(result) => match result {
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
            Err(ChatSubmitError::QueueFull) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "turn worker queue is full" })),
            )
                .into_response(),
        },
    }
}

/// `POST /chat` streaming variant — returns real SSE fused-runtime events.
///
/// The HTTP handler only authenticates/parses/enqueues. The dedicated worker
/// thread owns the non-`Send` fused runtime and forwards each [`FusedEvent`] into
/// this response body. Dropping the body closes the receiver; the worker then
/// drops the fused stream, cancelling the provider before receipt/journal/memory
/// side effects are committed.
async fn stream_chat(
    state: Arc<AppState>,
    message: String,
    session_id: Option<SessionId>,
) -> Response {
    let session_id = session_id.unwrap_or_default();
    let events = match state.stream_chat(message, session_id) {
        Ok(events) => events,
        Err(ChatSubmitError::WorkerGone) => {
            return (
                StatusCode::BAD_GATEWAY,
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse_frame(&json!({ "type": "error", "error": "turn worker is unavailable" })),
            )
                .into_response();
        }
        Err(ChatSubmitError::QueueFull) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse_frame(&json!({ "type": "error", "error": "turn worker queue is full" })),
            )
                .into_response();
        }
        Err(ChatSubmitError::Runtime(e)) => {
            return (
                StatusCode::BAD_GATEWAY,
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse_frame(&stream_error_json(&e)),
            )
                .into_response();
        }
    };

    let body = Body::from_stream(futures::stream::unfold(events, |mut events| async move {
        events.recv().await.map(|event| {
            let payload = match event {
                Ok(event) => fused_event_json(event),
                Err(err) => stream_error_json(&err),
            };
            let frame = Bytes::from(sse_frame(&payload));
            (Ok::<Bytes, Infallible>(frame), events)
        })
    }));

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONNECTION, "keep-alive"),
        ],
        body,
    )
        .into_response()
}

fn sse_frame(payload: &serde_json::Value) -> String {
    let json = serde_json::to_string(payload).expect("SSE payload serializes");
    format!("data: {json}\n\n")
}

fn stream_error_json(err: &RuntimeError) -> serde_json::Value {
    json!({
        "type": "error",
        "error": err.to_string(),
    })
}

fn fused_event_json(event: FusedEvent) -> serde_json::Value {
    match event {
        FusedEvent::StageStart { stage } => json!({
            "type": "stage_start",
            "stage": stage_label(stage),
        }),
        FusedEvent::StageEnd { stage, ok } => json!({
            "type": "stage_end",
            "stage": stage_label(stage),
            "ok": ok,
        }),
        FusedEvent::Content(text) => json!({
            "type": "content",
            "text": text,
        }),
        FusedEvent::ToolCallStart { id, name } => json!({
            "type": "tool_call_start",
            "id": id,
            "name": name,
        }),
        FusedEvent::ToolCallDelta { id, delta } => json!({
            "type": "tool_call_delta",
            "id": id,
            "delta": delta,
        }),
        FusedEvent::ToolCallResult { id, result } => json!({
            "type": "tool_call_result",
            "id": id,
            "result": result,
        }),
        FusedEvent::Usage(usage) => json!({
            "type": "usage",
            "usage": usage,
        }),
        FusedEvent::Receipt {
            receipt_id,
            chain_hash,
            cost_cents,
        } => json!({
            "type": "receipt",
            "receipt_id": receipt_id,
            "chain_hash": chain_hash,
            "cost_cents": cost_cents,
        }),
        FusedEvent::Finish(reason) => json!({
            "type": "finish",
            "reason": reason,
        }),
    }
}

fn stage_label(stage: StageKind) -> &'static str {
    match stage {
        StageKind::CapTokenVerify => "cap_token_verify",
        StageKind::CedarCheck => "cedar_check",
        StageKind::InjectionScan => "injection_scan",
        StageKind::CostGateAdmit => "cost_gate_admit",
        StageKind::ProviderStream => "provider_stream",
        StageKind::ToolExec => "tool_exec",
        StageKind::ReceiptMint => "receipt_mint",
        StageKind::CostGateFinalize => "cost_gate_finalize",
        StageKind::MemoryRecord => "memory_record",
        StageKind::JournalAppend => "journal_append",
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
    if body.len() > HTTP_BODY_LIMIT_BYTES {
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
    match tokio::time::timeout(
        HTTP_TURN_TIMEOUT,
        state.submit_chat(prompt, SessionId::new()),
    )
    .await
    {
        Err(_elapsed) => (
            StatusCode::GATEWAY_TIMEOUT,
            Json(AcpMessage::Response(AcpResponse::failure(
                request.id,
                AcpErrorObject::new(-32003, "turn processing timed out", None),
            ))),
        )
            .into_response(),
        Ok(result) => match result {
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
            Err(ChatSubmitError::QueueFull) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(AcpMessage::Response(AcpResponse::failure(
                    request.id,
                    AcpErrorObject::new(-32002, "turn worker queue is full", None),
                ))),
            )
                .into_response(),
        },
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
    const MAX_BEARER_TOKEN_LEN: usize = 4096;
    let presented = header_str(headers, "Authorization");
    let Some(token) = presented.strip_prefix("Bearer ") else {
        return Err(());
    };
    // ARD-485: reject attacker-controlled, oversized bearer values before any
    // constant-time comparison work. The previous implementation allocated two
    // vectors sized to the presented token for every configured token, so a
    // pre-auth request could force unbounded memory churn.
    if token.len() > MAX_BEARER_TOKEN_LEN {
        return Err(());
    }
    // Fail closed when no tokens are configured — every request is denied.
    if allowed_tokens.is_empty() {
        return Err(());
    }
    // Constant-time comparison to prevent timing side-channel attacks that
    // could leak the correct token byte-by-byte. Every configured token is
    // compared against a fixed-width, zero-padded buffer, and the length check is
    // folded into the constant-time result instead of branching on equality.
    let presented_bytes = token.as_bytes();
    let presented_padded = padded_bearer::<MAX_BEARER_TOKEN_LEN>(presented_bytes);
    let presented_len = presented_bytes.len() as u64;
    let mut found = subtle::Choice::from(0);
    for allowed in allowed_tokens {
        let allowed_bytes = allowed.as_bytes();
        let allowed_padded = padded_bearer::<MAX_BEARER_TOKEN_LEN>(allowed_bytes);
        let allowed_len = allowed_bytes.len() as u64;
        found |= presented_len.ct_eq(&allowed_len)
            & presented_padded.as_slice().ct_eq(allowed_padded.as_slice());
    }
    if found.into() { Ok(()) } else { Err(()) }
}

fn padded_bearer<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut out = [0_u8; N];
    for (idx, slot) in out.iter_mut().enumerate() {
        *slot = bytes.get(idx).copied().unwrap_or(0);
    }
    out
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

    // Unreachable in practice: this route is only mounted when Slack is enabled
    // (see `build_router`). Guard rather than panic if it is ever hit.
    let Some(slack) = state.slack() else {
        tracing::error!("slack event received but Slack is disabled");
        return StatusCode::NOT_FOUND.into_response();
    };

    match slack.parse_event(&slack_headers, body_str) {
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
