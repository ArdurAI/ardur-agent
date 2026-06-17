//! Cap-token and Cedar enforced memory control-plane operations.
//!
//! The raw [`MemoryRuntime`](crate::MemoryRuntime) remains a storage trait so
//! tests/backends can compose it freely. Operator-facing memory operations run
//! through [`MemoryControlPlane`], which enforces three production invariants:
//!
//! 1. the verified cap-token claims must include the needed `memory.read` or
//!    `memory.write` capability;
//! 2. the same request must be allowed by a Cedar policy bundle; and
//! 3. writes must carry a receipt id, so memory cards and tombstones are
//!    receipt-chained rather than unaudited side effects.

use ardur_cap_token::VerifiedClaims;
use ardur_cedar_policy::{
    ActionRef, CedarPolicyBundle, Decision, EvaluationContext, PolicyBundle, PrincipalRef,
    ResourceRef,
};
use uuid::Uuid;

use crate::{
    HolderId, InvalidationReason, MemoryCard, MemoryError, MemoryRecord, MemoryRuntime, ReceiptId,
    RecordId, Result, UnixTsMillis,
};

/// Capability string required for memory reads (`list` and `show`).
pub const MEMORY_READ_CAPABILITY: &str = "memory.read";
/// Capability string required for memory writes (`record` and `forget`).
pub const MEMORY_WRITE_CAPABILITY: &str = "memory.write";

/// High-level memory operation being authorized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryAction {
    /// List current memory cards for the verified subject/workspace.
    List,
    /// Show one memory card by id.
    Show,
    /// Record a new memory card.
    Record,
    /// Forget a memory by appending a receipt-chained tombstone.
    Forget,
}

impl MemoryAction {
    fn capability(self) -> &'static str {
        match self {
            Self::List | Self::Show => MEMORY_READ_CAPABILITY,
            Self::Record | Self::Forget => MEMORY_WRITE_CAPABILITY,
        }
    }

    fn cedar_action(self) -> &'static str {
        match self {
            Self::List => "Action::\"MemoryList\"",
            Self::Show => "Action::\"MemoryShow\"",
            Self::Record => "Action::\"MemoryRecord\"",
            Self::Forget => "Action::\"MemoryForget\"",
        }
    }
}

/// A thin authorization wrapper around any [`MemoryRuntime`].
pub struct MemoryControlPlane<'a, R: MemoryRuntime + ?Sized> {
    runtime: &'a R,
    policies: CedarPolicyBundle,
}

impl<'a, R: MemoryRuntime + ?Sized> MemoryControlPlane<'a, R> {
    /// Create a memory control plane over `runtime` with a Cedar policy bundle.
    #[must_use]
    pub fn new(runtime: &'a R, policies: CedarPolicyBundle) -> Self {
        Self { runtime, policies }
    }

    /// Record a new memory after cap-token, Cedar, subject, and receipt checks.
    ///
    /// # Errors
    /// Returns [`MemoryError`] if authorization fails, the record's subject does
    /// not match the verified holder, the record lacks a receipt, or the backing
    /// runtime rejects the write.
    pub fn record(&self, claims: &VerifiedClaims, rec: MemoryRecord) -> Result<RecordId> {
        self.authorize(MemoryAction::Record, claims, &rec.subject)?;
        require_same_subject(claims, &rec.subject)?;
        require_receipt(MemoryAction::Record, rec.source_receipt_id)?;
        self.runtime.record(rec)
    }

    /// List current memory cards for `subject` as of `as_of`.
    ///
    /// # Errors
    /// Returns [`MemoryError`] if cap-token/Cedar authorization fails or the
    /// requested subject does not match the verified holder.
    pub fn list(
        &self,
        claims: &VerifiedClaims,
        subject: &HolderId,
        as_of: UnixTsMillis,
    ) -> Result<Vec<MemoryCard>> {
        self.authorize(MemoryAction::List, claims, subject)?;
        require_same_subject(claims, subject)?;
        Ok(self
            .runtime
            .current_as_of(subject, as_of)
            .into_iter()
            .map(|rec| MemoryCard::from_record(&rec))
            .collect())
    }

    /// Show one live memory card by id for `subject`.
    ///
    /// # Errors
    /// Returns [`MemoryError`] if cap-token/Cedar authorization fails or the
    /// requested subject does not match the verified holder.
    pub fn show(
        &self,
        claims: &VerifiedClaims,
        subject: &HolderId,
        record_id: RecordId,
    ) -> Result<Option<MemoryCard>> {
        self.authorize(MemoryAction::Show, claims, subject)?;
        require_same_subject(claims, subject)?;
        Ok(self
            .runtime
            .history_of(record_id)
            .into_iter()
            .filter(|rec| rec.subject == *subject)
            .filter(|rec| rec.invalidation_time.is_none())
            .max_by_key(|rec| rec.recorded_at)
            .map(|rec| MemoryCard::from_record(&rec)))
    }

    /// Forget a memory by appending a receipt-chained invalidation record.
    ///
    /// # Errors
    /// Returns [`MemoryError`] if authorization fails, the record does not exist
    /// in `subject`, or the backing runtime rejects the tombstone append.
    pub fn forget(
        &self,
        claims: &VerifiedClaims,
        subject: &HolderId,
        record_id: RecordId,
        at: UnixTsMillis,
        receipt_id: ReceiptId,
    ) -> Result<()> {
        self.authorize(MemoryAction::Forget, claims, subject)?;
        require_same_subject(claims, subject)?;
        let target = self
            .runtime
            .history_of(record_id)
            .into_iter()
            .find(|rec| rec.record_id == record_id.0 && rec.subject == *subject)
            .ok_or(MemoryError::NotFound(record_id.0))?;
        let tombstone = MemoryRecord {
            record_id: Uuid::new_v4(),
            subject: target.subject.clone(),
            kind: target.kind,
            payload: serde_json::json!({
                "invalidates": target.record_id,
                "reason": InvalidationReason::UserCorrection,
                "source": "memory-control-plane",
                "workspace_id": subject.0,
            }),
            event_time: at,
            valid_from: at,
            valid_to: None,
            invalidation_time: Some(at),
            recorded_at: at,
            source_receipt_id: Some(receipt_id),
            correction_chain_root: target.correction_chain_root,
        };
        self.runtime.record(tombstone)?;
        Ok(())
    }

    fn authorize(
        &self,
        action: MemoryAction,
        claims: &VerifiedClaims,
        subject: &HolderId,
    ) -> Result<()> {
        let needed = action.capability();
        if !claims.tool_allowlist.iter().any(|tool| tool == needed) {
            return Err(MemoryError::CapabilityDenied {
                action,
                required: needed.to_string(),
            });
        }

        let decision = self.policies.evaluate(&EvaluationContext {
            principal: PrincipalRef(format!("User::\"{}\"", claims.subject.0)),
            action: ActionRef(action.cedar_action().to_string()),
            resource: ResourceRef(format!("Memory::\"{}\"", subject.0)),
            attributes: serde_json::json!({
                "subject": claims.subject.0,
                "audience": claims.audience,
                "workspace_id": subject.0,
                "capability": needed,
            }),
        });
        match decision {
            Decision::Allow { .. } => Ok(()),
            Decision::Deny { reason, .. } | Decision::Indeterminate { reason } => {
                Err(MemoryError::PolicyDenied { action, reason })
            }
        }
    }
}

fn require_same_subject(claims: &VerifiedClaims, subject: &HolderId) -> Result<()> {
    if claims.subject.0 == subject.0 {
        Ok(())
    } else {
        Err(MemoryError::SubjectMismatch {
            claim_subject: claims.subject.0.clone(),
            memory_subject: subject.0.clone(),
        })
    }
}

fn require_receipt(action: MemoryAction, receipt: Option<ReceiptId>) -> Result<()> {
    if receipt.is_some() {
        Ok(())
    } else {
        Err(MemoryError::ReceiptRequired { action })
    }
}
