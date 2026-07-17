//! Read-only access to ardur-server's hash-chained receipt log.
//!
//! ardur-server appends one **compact JWS** per line to `chain.jsonl`. The
//! signed body is the JWS's middle (payload) segment, base64url-encoded; we
//! decode it into an [`ardur_receipt::ReceiptBody`] exactly as the fused
//! runtime's own loader does. We never verify a signature (the admin-ui has no
//! JWKS and no reason to) and we never write — this is a pure decode.
//!
//! ## The "provider" dimension
//!
//! A receipt minted at or after §11.14b carries an explicit
//! [`provider`](ardur_receipt::ReceiptBody::provider) field (e.g. `"anthropic"`),
//! and the admin-ui prefers it. Receipts minted before §11.14b leave it `None`;
//! for those we fall back to the receipt **verb** (`verb.object.state.vN`, e.g.
//! `llm.completion.minted.v1`) as the closest available grouping key. Either way
//! the value surfaces as the "provider" dimension in the receipts feed and the
//! cost-by-provider breakdown. This is documented in the README.

use std::fs;
use std::path::{Path, PathBuf};

use ardur_receipt::ReceiptBody;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use serde::Serialize;

/// One receipt loaded from disk: its compact JWS and the decoded body.
#[derive(Debug, Clone)]
pub struct LoadedReceipt {
    /// The compact JWS line (`header.payload.sig`).
    pub jws_compact: String,
    /// The body decoded from the JWS payload segment.
    pub body: ReceiptBody,
}

impl LoadedReceipt {
    /// The "provider" label: the explicit §11.14b
    /// [`provider`](ardur_receipt::ReceiptBody::provider) field when present,
    /// otherwise the verb as a fallback (see module docs).
    pub fn provider(&self) -> &str {
        self.body
            .provider
            .as_deref()
            .unwrap_or_else(|| self.body.verb.as_str())
    }
}

/// A compact summary of a receipt for the feed / list endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct ReceiptSummary {
    /// The receipt id (UUID).
    pub receipt_id: String,
    /// The receipt verb, surfaced as the provider dimension.
    pub provider: String,
    /// When the receipted action occurred (ms since epoch).
    pub issued_at_ms: u64,
    /// Monetary cost in cents.
    pub cents: u64,
    /// Input tokens billed.
    pub tokens_in: u64,
    /// Output tokens billed.
    pub tokens_out: u64,
    /// Names of the tools this turn invoked.
    pub tool_calls: Vec<String>,
    /// Number of tool invocations.
    pub tool_call_count: usize,
}

impl From<&LoadedReceipt> for ReceiptSummary {
    fn from(r: &LoadedReceipt) -> Self {
        let b = &r.body;
        Self {
            receipt_id: b.receipt_id.to_string(),
            provider: r.provider().to_string(),
            issued_at_ms: b.issued_at.0,
            cents: b.cost.cents,
            tokens_in: b.cost.tokens_in,
            tokens_out: b.cost.tokens_out,
            tool_calls: b.tool_calls.iter().map(|t| t.tool_name.clone()).collect(),
            tool_call_count: b.tool_calls.len(),
        }
    }
}

/// Resolve the receipt-store argument to the chain file. A path that is itself a
/// file (or ends in `.jsonl`) is used verbatim; otherwise `chain.jsonl` under
/// the given directory — matching ardur-server's `<data>/receipts/chain.jsonl`.
pub fn chain_path(receipt_store: &Path) -> PathBuf {
    if receipt_store.is_file() || receipt_store.extension().is_some_and(|e| e == "jsonl") {
        receipt_store.to_path_buf()
    } else {
        receipt_store.join("chain.jsonl")
    }
}

/// Decode the [`ReceiptBody`] from a compact JWS's payload (middle) segment.
fn decode_body(jws_compact: &str) -> anyhow::Result<ReceiptBody> {
    let payload_b64 = jws_compact
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("not a three-segment compact JWS"))?;
    let payload = B64URL
        .decode(payload_b64)
        .map_err(|e| anyhow::anyhow!("payload base64url: {e}"))?;
    serde_json::from_slice(&payload).map_err(|e| anyhow::anyhow!("payload json: {e}"))
}

/// Load every receipt in append (chain) order. A missing chain file is an empty
/// log. A line that fails to decode is skipped (logged) rather than failing the
/// whole read, so a single corrupt trailing write can't blind the dashboard.
pub fn load_chain(receipt_store: &Path) -> anyhow::Result<Vec<LoadedReceipt>> {
    let path = chain_path(receipt_store);
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        match decode_body(line) {
            Ok(body) => out.push(LoadedReceipt {
                jws_compact: line.to_string(),
                body,
            }),
            Err(e) => tracing::warn!(error = %e, "skipping undecodable receipt line"),
        }
    }
    Ok(out)
}

/// The most recent `n` receipts, newest first.
pub fn recent(receipt_store: &Path, n: usize) -> anyhow::Result<Vec<LoadedReceipt>> {
    let mut chain = load_chain(receipt_store)?;
    chain.reverse();
    chain.truncate(n);
    Ok(chain)
}

/// Find one receipt by its UUID string.
pub fn find_by_id(receipt_store: &Path, id: &str) -> anyhow::Result<Option<LoadedReceipt>> {
    Ok(load_chain(receipt_store)?
        .into_iter()
        .find(|r| r.body.receipt_id.to_string() == id))
}
