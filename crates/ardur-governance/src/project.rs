//! Project a governed tool-call into an MCEP Execution Receipt claim set.
//!
//! This is the seam: the agent's authorization outcome (from the cap-token
//! verifier / Cedar) plus the verified grant claims and the normalized
//! invocation become an ER, ready to sign and chain. The native receipt chain
//! (`ardur-receipt`, single-writer under `commit_lock`) is untouched — the ER
//! is a mirror record projected from the same facts.

use std::collections::BTreeMap;

use ardur_cap_token::{CapTokenError, VerifiedClaims};
use chrono::{TimeZone, Utc};
use serde_json::{Value, json};

use crate::er::{
    ActionClass, Canonicalization, DigestAlg, DigestObject, DigestScope, EvidenceLevel,
    ExecutionReceipt, PolicyDecision, PublicDenialReason, SideEffectClass, Verdict,
};
use crate::error::GovernanceError;
use crate::hash::{sha256_b64url, sha256_hex};
use crate::jcs;
use crate::sign::SignedExecutionReceipt;

/// The authorization outcome for the step, already reduced to the tri-state ER
/// verdict plus (for non-compliant) the fixed denial vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthOutcome {
    /// Sufficient evidence; within policy.
    Compliant,
    /// Sufficient evidence; a policy/integrity violation.
    Violation {
        /// User-facing denial reason.
        public: PublicDenialReason,
        /// Audit-only denial code.
        internal: String,
    },
    /// Could not honestly determine compliance.
    InsufficientEvidence {
        /// Audit-only denial code (e.g. `telemetry_missing`).
        internal: String,
    },
}

impl AuthOutcome {
    /// Map a cap-token verification failure to an ER outcome, following the
    /// verifier-contract §9 fail-closed table.
    pub fn from_cap_token_error(err: &CapTokenError) -> Self {
        let (public, internal) = match err {
            CapTokenError::Expired => (PublicDenialReason::PolicyDenied, "grant_expired"),
            CapTokenError::AudienceMismatch => {
                (PublicDenialReason::PolicyDenied, "audience_mismatch")
            }
            CapTokenError::BudgetExhausted => {
                (PublicDenialReason::BudgetExhausted, "budget_exhausted")
            }
            CapTokenError::ToolNotAllowed => (PublicDenialReason::PolicyDenied, "tool_not_allowed"),
            CapTokenError::Revoked => (PublicDenialReason::Revoked, "revoked"),
            CapTokenError::SignatureInvalid => {
                (PublicDenialReason::ChainInvalid, "signature_invalid")
            }
            CapTokenError::Malformed(_) => (PublicDenialReason::ChainInvalid, "malformed_token"),
        };
        AuthOutcome::Violation {
            public,
            internal: internal.to_string(),
        }
    }

    fn verdict(&self) -> Verdict {
        match self {
            AuthOutcome::Compliant => Verdict::Compliant,
            AuthOutcome::Violation { .. } => Verdict::Violation,
            AuthOutcome::InsufficientEvidence { .. } => Verdict::InsufficientEvidence,
        }
    }
}

/// The normalized tool invocation being governed.
#[derive(Clone, Debug)]
pub struct ToolInvocation<'a> {
    /// Tool / API / capability invoked.
    pub tool: &'a str,
    /// High-level action family.
    pub action_class: ActionClass,
    /// Normalized target (1..=2048 chars).
    pub target: &'a str,
    /// Coarse resource category.
    pub resource_family: &'a str,
    /// Side-effect family.
    pub side_effect_class: SideEffectClass,
    /// Normalized invocation arguments (hashed under JCS).
    pub arguments: &'a Value,
}

/// Per-step identity + lineage context for the emitted receipt.
#[derive(Clone, Debug)]
pub struct StepContext<'a> {
    /// Identity of the verifier emitting the receipt.
    pub verifier_id: &'a str,
    /// Token issuer (SHOULD equal `verifier_id`).
    pub iss: &'a str,
    /// Stable run / trace-segment id (ER `idString`).
    pub trace_id: &'a str,
    /// Fresh per-run nonce (base64url, 16..=128 chars).
    pub run_nonce: &'a str,
    /// Stable step id.
    pub step_id: &'a str,
    /// Step time in Unix milliseconds.
    pub timestamp_millis: u64,
    /// Receipt lifetime in seconds (`exp = iat + ttl_secs`).
    pub ttl_secs: u64,
    /// Assurance level to stamp.
    pub evidence_level: EvidenceLevel,
    /// Preceding receipt in this lineage (`None` at the root).
    pub parent: Option<&'a SignedExecutionReceipt>,
}

/// Project a governed step into an [`ExecutionReceipt`]. `grant_id` is the
/// cap-token `token_id`; `actor` and `budget_remaining` come from the verified
/// claims.
pub fn project_execution_receipt(
    claims: &VerifiedClaims,
    call: &ToolInvocation,
    outcome: &AuthOutcome,
    step: &StepContext,
) -> Result<ExecutionReceipt, GovernanceError> {
    validate_id_string("trace_id", step.trace_id)?;
    validate_len("step_id", step.step_id, 1, 256)?; // nonEmptyString
    validate_run_nonce(step.run_nonce)?;
    validate_id_string("grant_id", &claims.token_id.to_string())?;
    validate_len("target", call.target, 1, 2048)?;

    // arguments_hash over the JCS-canonical normalized arguments.
    let arguments_hash = sha256_hex(&jcs::to_canonical_bytes(call.arguments));

    // invocation_digest over the JCS-canonical normalized invocation envelope.
    let envelope = json!({
        "action_class": serde_plain(&call.action_class),
        "arguments": call.arguments,
        "grant_id": claims.token_id.to_string(),
        "resource_family": call.resource_family,
        "side_effect_class": serde_plain(&call.side_effect_class),
        "target": call.target,
        "tool": call.tool,
    });
    let invocation_digest = DigestObject {
        alg: DigestAlg::Sha256,
        canonicalization: Some(Canonicalization::JcsRfc8785),
        scope: Some(DigestScope::NormalizedInput),
        value: sha256_b64url(&jcs::to_canonical_bytes(&envelope)),
    };

    let timestamp = rfc3339_from_millis(step.timestamp_millis)?;
    let iat = step.timestamp_millis / 1000;
    let exp = iat + step.ttl_secs.max(1);

    let parent_receipt_hash = step.parent.map(|p| p.receipt_hash());
    let parent_receipt_id = parent_receipt_hash.as_ref().map(|h| h[..16].to_string());

    // Stable, collision-resistant receipt_id / jti (mirrors the reference impl:
    // a hash over the id-free step material).
    let stable_seed = format!(
        "{}|{}|{}|{}|{}",
        step.trace_id, step.run_nonce, step.step_id, claims.token_id, invocation_digest.value
    );
    let receipt_id = format!("er:{}", &sha256_hex(stable_seed.as_bytes())[..40]);
    let jti = format!(
        "er:jti:{}",
        &sha256_hex(format!("{receipt_id}|{iat}").as_bytes())[..48]
    );

    let verdict = outcome.verdict();
    let (reason, decision, public_denial_reason, internal_denial_code) = match outcome {
        AuthOutcome::Compliant => (
            "within policy".to_string(),
            "permit".to_string(),
            None,
            None,
        ),
        AuthOutcome::Violation { public, internal } => (
            format!("violation: {internal}"),
            "deny".to_string(),
            Some(*public),
            Some(internal.clone()),
        ),
        AuthOutcome::InsufficientEvidence { internal } => (
            format!("insufficient evidence: {internal}"),
            "insufficient".to_string(),
            Some(PublicDenialReason::InsufficientEvidence),
            Some(internal.clone()),
        ),
    };

    let mut budget_remaining = BTreeMap::new();
    budget_remaining.insert("cost".to_string(), claims.budget_remaining);

    let policy_decisions = vec![PolicyDecision {
        backend: "cap-token".to_string(),
        decision,
        reason: Some(reason.clone()),
        eval_ms: None,
    }];

    let receipt = ExecutionReceipt {
        receipt_id,
        grant_id: claims.token_id.to_string(),
        parent_receipt_id,
        parent_receipt_hash,
        actor: claims.subject.0.clone(),
        verifier_id: step.verifier_id.to_string(),
        trace_id: step.trace_id.to_string(),
        run_nonce: step.run_nonce.to_string(),
        step_id: step.step_id.to_string(),
        invocation_digest,
        tool: call.tool.to_string(),
        action_class: call.action_class,
        target: call.target.to_string(),
        resource_family: call.resource_family.to_string(),
        side_effect_class: call.side_effect_class,
        verdict,
        evidence_level: step.evidence_level,
        reason,
        policy_decisions,
        arguments_hash,
        budget_remaining,
        timestamp,
        iss: step.iss.to_string(),
        iat,
        exp,
        jti,
        public_denial_reason,
        internal_denial_code,
    };

    // Enforce the schema's conditional invariant before anyone signs it.
    check_verdict_invariant(&receipt)?;
    Ok(receipt)
}

/// The schema `allOf` invariant: `compliant` ⇒ no denial fields;
/// `violation`/`insufficient_evidence` ⇒ both denial fields present.
pub fn check_verdict_invariant(er: &ExecutionReceipt) -> Result<(), GovernanceError> {
    match er.verdict {
        Verdict::Compliant => {
            if er.public_denial_reason.is_some() || er.internal_denial_code.is_some() {
                return Err(GovernanceError::VerdictInvariant(
                    "compliant receipt must not carry denial fields".to_string(),
                ));
            }
        }
        Verdict::Violation | Verdict::InsufficientEvidence => {
            if er.public_denial_reason.is_none() || er.internal_denial_code.is_none() {
                return Err(GovernanceError::VerdictInvariant(
                    "non-compliant receipt must carry both public_denial_reason and \
                     internal_denial_code"
                        .to_string(),
                ));
            }
        }
    }
    Ok(())
}

/// The lowercase snake_case wire token for an ER enum value (via its serde
/// representation), without the surrounding JSON quotes.
fn serde_plain<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn rfc3339_from_millis(millis: u64) -> Result<String, GovernanceError> {
    let secs = (millis / 1000) as i64;
    let nsecs = ((millis % 1000) * 1_000_000) as u32;
    match Utc.timestamp_opt(secs, nsecs).single() {
        Some(dt) => Ok(dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        None => Err(GovernanceError::InvalidClaim(format!(
            "timestamp_millis {millis} is out of range"
        ))),
    }
}

fn validate_id_string(field: &str, value: &str) -> Result<(), GovernanceError> {
    let len = value.chars().count();
    if !(8..=64).contains(&len) {
        return Err(GovernanceError::InvalidClaim(format!(
            "{field} must be 8..=64 chars (idString), got {len}"
        )));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'))
    {
        return Err(GovernanceError::InvalidClaim(format!(
            "{field} contains a character outside the idString set [A-Za-z0-9._:/-]"
        )));
    }
    Ok(())
}

fn validate_run_nonce(value: &str) -> Result<(), GovernanceError> {
    let len = value.chars().count();
    if !(16..=128).contains(&len) {
        return Err(GovernanceError::InvalidClaim(format!(
            "run_nonce must be 16..=128 chars (base64url), got {len}"
        )));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err(GovernanceError::InvalidClaim(
            "run_nonce contains a character outside base64url [A-Za-z0-9_-]".to_string(),
        ));
    }
    Ok(())
}

fn validate_len(field: &str, value: &str, min: usize, max: usize) -> Result<(), GovernanceError> {
    let len = value.chars().count();
    if !(min..=max).contains(&len) {
        return Err(GovernanceError::InvalidClaim(format!(
            "{field} length {len} outside {min}..={max}"
        )));
    }
    Ok(())
}
