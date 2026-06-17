//! Receipts for browser automation actions.
//!
//! Every browser UI action (navigate, click, type, screenshot, extract) is
//! receipted for audit. Receipts are signed and chained.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// A receipt for a single browser action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserReceipt {
    /// The action type (navigate, click, type, screenshot, extract).
    pub action: String,
    /// The target URL or selector.
    pub target: String,
    /// Whether the action was permitted by policy.
    pub permitted: bool,
    /// Human-readable reason if denied.
    pub denial_reason: Option<String>,
    /// Unix timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// A unique receipt id.
    pub receipt_id: String,
    /// The parent receipt id (for chaining).
    pub parent_receipt_id: Option<String>,
}

impl BrowserReceipt {
    /// Create a new browser receipt.
    #[must_use]
    pub fn new(
        action: impl Into<String>,
        target: impl Into<String>,
        permitted: bool,
        denial_reason: Option<String>,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            action: action.into(),
            target: target.into(),
            permitted,
            denial_reason,
            timestamp_ms: now,
            receipt_id: format!("br-{}", now),
            parent_receipt_id: None,
        }
    }

    /// Set the parent receipt id to chain receipts.
    #[must_use]
    pub fn with_parent(mut self, parent_id: impl Into<String>) -> Self {
        self.parent_receipt_id = Some(parent_id.into());
        self
    }
}

/// A collection of browser action receipts with chain verification.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserActionReceipt {
    /// The ordered list of receipts.
    pub receipts: Vec<BrowserReceipt>,
}

impl BrowserActionReceipt {
    /// Create an empty receipt chain.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a receipt to the chain, linking it to the previous receipt.
    pub fn push(&mut self, mut receipt: BrowserReceipt) {
        if let Some(last) = self.receipts.last() {
            receipt.parent_receipt_id = Some(last.receipt_id.clone());
        }
        self.receipts.push(receipt);
    }

    /// Verify the chain is intact (every receipt's parent exists).
    #[must_use]
    pub fn verify_chain(&self) -> bool {
        for (i, receipt) in self.receipts.iter().enumerate().skip(1) {
            let expected_parent = self.receipts[i - 1].receipt_id.clone();
            if receipt.parent_receipt_id.as_ref() != Some(&expected_parent) {
                return false;
            }
        }
        true
    }

    /// The number of receipts in the chain.
    #[must_use]
    pub fn len(&self) -> usize {
        self.receipts.len()
    }

    /// Whether the chain is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_new() {
        let r = BrowserReceipt::new("navigate", "https://example.com", true, None);
        assert_eq!(r.action, "navigate");
        assert_eq!(r.target, "https://example.com");
        assert!(r.permitted);
        assert!(r.denial_reason.is_none());
        assert!(r.timestamp_ms > 0);
    }

    #[test]
    fn receipt_with_parent() {
        let r = BrowserReceipt::new("click", "#btn", true, None)
            .with_parent("parent-123");
        assert_eq!(r.parent_receipt_id, Some("parent-123".to_string()));
    }

    #[test]
    fn action_receipt_chain() {
        let mut chain = BrowserActionReceipt::new();
        let r1 = BrowserReceipt::new("navigate", "https://example.com", true, None);
        let r1_id = r1.receipt_id.clone();
        chain.push(r1);

        let r2 = BrowserReceipt::new("click", "#btn", true, None);
        chain.push(r2);

        assert_eq!(chain.len(), 2);
        assert!(chain.verify_chain());
        assert_eq!(chain.receipts[1].parent_receipt_id, Some(r1_id));
    }

    #[test]
    fn broken_chain_detected() {
        let mut chain = BrowserActionReceipt::new();
        let mut r1 = BrowserReceipt::new("navigate", "https://a.com", true, None);
        r1.receipt_id = "id-1".to_string();
        chain.push(r1);

        let mut r2 = BrowserReceipt::new("click", "#btn", true, None);
        r2.parent_receipt_id = Some("wrong-parent".to_string());
        chain.receipts.push(r2);

        assert!(!chain.verify_chain());
    }
}
