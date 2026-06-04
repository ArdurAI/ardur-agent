//! The read-only HTTP surface.
//!
//! Every route is a `GET`. There is deliberately **no** `POST`/`PUT`/`DELETE`/
//! `PATCH` handler anywhere — the binary cannot mutate the artifacts it reads.
//! [`build_router`] assembles the routes and, when configured, the Basic-auth
//! gate.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use maud::Markup;
use serde::{Deserialize, Serialize};

use crate::costs::{self, CostsReport};
use crate::journal::{self, JournalPage, SessionSummary};
use crate::memory::{self, MemoryRecent};
use crate::receipts::{self, ReceiptSummary};
use crate::state::SharedState;
use crate::{auth, html};

/// An error from a handler — rendered as a `500` with the message. Reads that
/// simply find nothing (missing session, unknown receipt) return `404` instead,
/// handled at the call site.
struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self.0, "admin-ui handler error");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("error: {}", self.0),
        )
            .into_response()
    }
}

/// The wall-clock now, in milliseconds since the epoch (UTC).
fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// `GET /healthz` — readiness check.
async fn healthz() -> &'static str {
    "ok"
}

/// `GET /` — the server-rendered dashboard.
async fn dashboard(State(state): State<SharedState>) -> Result<Markup, ApiError> {
    let report = costs::report(&state.receipt_store, &state.journal_dir, now_ms())?;
    let sessions = journal::list_sessions(&state.journal_dir)?;
    let recent = receipts::recent(&state.receipt_store, 50)?;
    let receipt_summaries: Vec<ReceiptSummary> = recent.iter().map(ReceiptSummary::from).collect();
    Ok(html::dashboard(&report, &sessions, &receipt_summaries))
}

/// `GET /api/sessions`
async fn list_sessions(
    State(state): State<SharedState>,
) -> Result<Json<Vec<SessionSummary>>, ApiError> {
    Ok(Json(journal::list_sessions(&state.journal_dir)?))
}

/// Pagination query for a journal page.
#[derive(Debug, Deserialize)]
struct JournalQuery {
    /// Page size (default 100).
    limit: Option<usize>,
    /// 0-based start within the journal. When omitted, the page is the tail
    /// (the last `limit` entries).
    offset: Option<usize>,
}

/// `GET /api/sessions/:id/journal?limit=&offset=`
async fn session_journal(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Query(q): Query<JournalQuery>,
) -> Result<Json<JournalPage>, ApiError> {
    let limit = q.limit.unwrap_or(100);
    Ok(Json(journal::page(
        &state.journal_dir,
        &id,
        limit,
        q.offset,
    )?))
}

/// `GET /api/receipts` — the most recent 50, summarized.
async fn list_receipts(
    State(state): State<SharedState>,
) -> Result<Json<Vec<ReceiptSummary>>, ApiError> {
    let recent = receipts::recent(&state.receipt_store, 50)?;
    Ok(Json(recent.iter().map(ReceiptSummary::from).collect()))
}

/// One receipt, in full: its decoded body plus the canonical compact JWS.
#[derive(Debug, Serialize)]
struct FullReceipt {
    /// The receipt body.
    body: ardur_receipt::ReceiptBody,
    /// The compact JWS line it was loaded from.
    jws_compact: String,
}

/// `GET /api/receipts/:id`
async fn receipt_by_id(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    match receipts::find_by_id(&state.receipt_store, &id)? {
        Some(r) => Ok(Json(FullReceipt {
            body: r.body,
            jws_compact: r.jws_compact,
        })
        .into_response()),
        None => Ok((StatusCode::NOT_FOUND, "receipt not found").into_response()),
    }
}

/// `GET /api/costs`
async fn costs_report(State(state): State<SharedState>) -> Result<Json<CostsReport>, ApiError> {
    Ok(Json(costs::report(
        &state.receipt_store,
        &state.journal_dir,
        now_ms(),
    )?))
}

/// `GET /api/memory/recent`
async fn memory_recent(State(state): State<SharedState>) -> Result<Json<MemoryRecent>, ApiError> {
    match &state.memory {
        Some(source) => Ok(Json(memory::recent(source, 20).await?)),
        None => Ok(Json(MemoryRecent::disabled())),
    }
}

/// Assemble the read-only router, layering the Basic-auth gate (a no-op unless
/// `AppState::basic_auth` is set).
pub fn build_router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/healthz", get(healthz))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/:id/journal", get(session_journal))
        .route("/api/receipts", get(list_receipts))
        .route("/api/receipts/:id", get(receipt_by_id))
        .route("/api/costs", get(costs_report))
        .route("/api/memory/recent", get(memory_recent))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .with_state(state)
}
