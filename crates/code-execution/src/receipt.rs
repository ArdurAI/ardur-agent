//! Receipts for `code.exec` dispatches.
//!
//! Every dispatch emits a `requested` receipt before the adapter runs, then
//! exactly one of `completed` or `failed` after. A denied tool-callback
//! intent (see [`crate::CodeExecutionCaveat::attenuate`]) emits one
//! `tool_denied` receipt per denied tool, each parented to the dispatch's
//! `requested` receipt — the receipt set is a forest rooted at the request,
//! matching the §6.7 blueprint's "receipt chain is a forest" invariant.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static RECEIPT_COUNTER: AtomicU64 = AtomicU64::new(1);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The event a [`CodeExecutionReceipt`] records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptKind {
    /// `code.exec.requested.v1` — emitted once, before the adapter runs.
    Requested,
    /// `code.exec.completed.v1` — the adapter ran to completion (any exit
    /// code counts as "completed"; a nonzero exit is not itself a failure of
    /// the dispatch).
    Completed,
    /// `code.exec.failed.v1` — the dispatch itself failed (spawn error,
    /// timeout, injection block) before/without a usable exit code.
    Failed,
    /// `code.exec.tool_denied.v1` — a tool named in the request's
    /// `tool_allowlist` was outside the cap-token caveat's permitted set.
    ToolDenied,
}

impl ReceiptKind {
    /// The receipt's schema name, matching the blueprint's `code.exec.*.v1`
    /// family.
    #[must_use]
    pub fn schema_name(self) -> &'static str {
        match self {
            Self::Requested => "code.exec.requested.v1",
            Self::Completed => "code.exec.completed.v1",
            Self::Failed => "code.exec.failed.v1",
            Self::ToolDenied => "code.exec.tool_denied.v1",
        }
    }
}

/// A single receipt in a `code.exec` dispatch's receipt forest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodeExecutionReceipt {
    /// This receipt's schema-qualified kind.
    pub kind: ReceiptKind,
    /// The language the dispatch ran (or attempted to run).
    pub language: String,
    /// Free-form detail — the failure reason, the denied tool name, or the
    /// truncated exit summary, depending on `kind`.
    pub detail: String,
    /// Unix timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// A unique receipt id.
    pub receipt_id: String,
    /// The parent receipt id this receipt anchors to (the dispatch's
    /// `Requested` receipt, for every non-`Requested` kind).
    pub parent_receipt_id: Option<String>,
}

impl CodeExecutionReceipt {
    /// Mint a fresh, unparented receipt (used for the `Requested` kind that
    /// roots the forest).
    #[must_use]
    pub fn new(kind: ReceiptKind, language: impl Into<String>, detail: impl Into<String>) -> Self {
        let now = now_ms();
        let seq = RECEIPT_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            kind,
            language: language.into(),
            detail: detail.into(),
            timestamp_ms: now,
            receipt_id: format!("cx-{now}-{seq}"),
            parent_receipt_id: None,
        }
    }

    /// Anchor this receipt to a parent (typically the dispatch's `Requested`
    /// receipt id).
    #[must_use]
    pub fn with_parent(mut self, parent_id: impl Into<String>) -> Self {
        self.parent_receipt_id = Some(parent_id.into());
        self
    }

    /// Render this receipt as the JSON object folded into
    /// [`ardur_tool_registry::ToolOutput::receipt_data`].
    #[must_use]
    pub fn to_receipt_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.receipt_id,
            "parent_id": self.parent_receipt_id,
            "schema": self.kind.schema_name(),
            "language": self.language,
            "detail": self.detail,
            "timestamp_ms": self.timestamp_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_names_match_the_blueprint_family() {
        assert_eq!(
            ReceiptKind::Requested.schema_name(),
            "code.exec.requested.v1"
        );
        assert_eq!(
            ReceiptKind::Completed.schema_name(),
            "code.exec.completed.v1"
        );
        assert_eq!(ReceiptKind::Failed.schema_name(), "code.exec.failed.v1");
        assert_eq!(
            ReceiptKind::ToolDenied.schema_name(),
            "code.exec.tool_denied.v1"
        );
    }

    #[test]
    fn child_receipts_chain_to_the_requested_parent() {
        let requested = CodeExecutionReceipt::new(ReceiptKind::Requested, "bash", "dispatch");
        let completed = CodeExecutionReceipt::new(ReceiptKind::Completed, "bash", "exit=0")
            .with_parent(requested.receipt_id.clone());
        assert_eq!(
            completed.parent_receipt_id.as_deref(),
            Some(requested.receipt_id.as_str())
        );
        assert!(requested.parent_receipt_id.is_none());
    }

    #[test]
    fn receipt_ids_are_unique() {
        let a = CodeExecutionReceipt::new(ReceiptKind::Requested, "bash", "a");
        let b = CodeExecutionReceipt::new(ReceiptKind::Requested, "bash", "b");
        assert_ne!(a.receipt_id, b.receipt_id);
    }
}
