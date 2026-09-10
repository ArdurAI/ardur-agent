//! The MCEP **Execution Receipt (ER) v0.1** claim set.
//!
//! Field-for-field with `docs/specs/execution-receipt-v0.1.schema.json` in the
//! Ardur repo: 25 required claims plus the optional MIC-Evidence claims this
//! crate populates. The top-level schema is `additionalProperties: false`, so
//! every optional field is `skip_serializing_if` — an unset field emits no key.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// High-level action family for the evaluated step (schema `action_class`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionClass {
    /// Read-only search / retrieval.
    Search,
    /// Read a resource.
    Read,
    /// Mutate a resource.
    Write,
    /// Structured query.
    Query,
    /// Delegate authority to a child grant.
    Delegate,
    /// Send data outward.
    Send,
    /// Summarize observed content.
    Summarize,
    /// Passive observation.
    Observe,
}

/// Side-effect family of the step (schema `side_effect_class`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectClass {
    /// No side effect.
    None,
    /// Writes local/internal state.
    InternalWrite,
    /// Sends data to an external destination.
    ExternalSend,
    /// Changes durable state.
    StateChange,
}

/// Tri-state verifier result (schema `verdict`). `InsufficientEvidence` is an
/// honesty outcome and MUST NOT be collapsed into either other value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Sufficient evidence; within policy.
    Compliant,
    /// Sufficient evidence; policy or integrity violation.
    Violation,
    /// Could not honestly determine compliance.
    InsufficientEvidence,
}

/// Assurance level of the emitted receipt (schema `evidence_level`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceLevel {
    /// Signed only by the emitting verifier.
    SelfSigned,
    /// Independently countersigned out of band.
    CounterSigned,
    /// Anchored in an append-only transparency log.
    TransparencyLogged,
}

/// Coarse user-facing denial vocabulary (schema `public_denial_reason`). Absent
/// for `compliant` receipts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicDenialReason {
    /// Blocked by policy.
    PolicyDenied,
    /// A lineage budget was exhausted.
    BudgetExhausted,
    /// Required evidence missing/hidden/inconsistent.
    InsufficientEvidence,
    /// Grant or mission revoked.
    Revoked,
    /// Receipt/credential chain invalid.
    ChainInvalid,
}

/// Digest algorithm for a [`DigestObject`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DigestAlg {
    /// SHA-256.
    #[serde(rename = "sha-256")]
    Sha256,
    /// SHA-384.
    #[serde(rename = "sha-384")]
    Sha384,
    /// SHA-512.
    #[serde(rename = "sha-512")]
    Sha512,
}

/// Canonicalization applied before hashing a [`DigestObject`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Canonicalization {
    /// RFC 8785 JSON Canonicalization Scheme.
    #[serde(rename = "jcs-rfc8785")]
    JcsRfc8785,
    /// No canonicalization (raw bytes hashed).
    #[serde(rename = "none")]
    None,
}

/// What a [`DigestObject`] measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestScope {
    /// Result material.
    Result,
    /// Normalized verifier input.
    NormalizedInput,
    /// A measurement.
    Measurement,
    /// Deployment-defined.
    Custom,
}

/// A digest of a nested object (schema `digestObject`): `alg` + base64url
/// `value`, with optional `canonicalization` and `scope`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestObject {
    /// Hash algorithm.
    pub alg: DigestAlg,
    /// Canonicalization applied before hashing.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub canonicalization: Option<Canonicalization>,
    /// What was hashed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub scope: Option<DigestScope>,
    /// Base64url (no pad) of the raw digest bytes.
    pub value: String,
}

/// One policy-engine decision contributing to the verdict (schema
/// `policyDecision`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyDecision {
    /// The engine that produced the decision (e.g. `cap-token`, `cedar`).
    pub backend: String,
    /// The engine-local decision string (e.g. `permit`, `deny`).
    pub decision: String,
    /// Optional human-readable reason.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
    /// Optional evaluation time in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub eval_ms: Option<f64>,
}

/// The MCEP Execution Receipt v0.1 claim set. Serialize this to JSON and it
/// validates against `execution-receipt-v0.1.schema.json`; it is then carried
/// inside an ES256 JWS with `typ=application/ardur.er+jwt`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecutionReceipt {
    /// Stable identifier for this receipt as an evidence object.
    pub receipt_id: String,
    /// Governing delegation-grant id; in v0.1 the AAT `jti` (here the cap-token
    /// token id).
    pub grant_id: String,
    /// Immediately preceding receipt id in this lineage; `null` at the root.
    pub parent_receipt_id: Option<String>,
    /// Hex SHA-256 of the preceding signed ER JWT; `null` at the root.
    pub parent_receipt_hash: Option<String>,
    /// Identity of the actor that executed the step.
    pub actor: String,
    /// Identity of the verifier that emitted the receipt.
    pub verifier_id: String,
    /// Stable identifier for the governed run / trace segment.
    pub trace_id: String,
    /// Fresh per-run nonce (base64url, 16..=128 chars).
    pub run_nonce: String,
    /// Stable identifier for the evaluated step.
    pub step_id: String,
    /// Digest of the normalized invocation envelope.
    pub invocation_digest: DigestObject,
    /// Tool / API / capability invoked.
    pub tool: String,
    /// High-level action family.
    pub action_class: ActionClass,
    /// Normalized target string (1..=2048 chars).
    pub target: String,
    /// Coarse resource category used by MIC policy.
    pub resource_family: String,
    /// Side-effect family.
    pub side_effect_class: SideEffectClass,
    /// Tri-state verifier result.
    pub verdict: Verdict,
    /// Assurance level of this receipt.
    pub evidence_level: EvidenceLevel,
    /// Audit-facing explanation (1..=4096 chars).
    pub reason: String,
    /// Per-policy-engine decisions contributing to the verdict.
    pub policy_decisions: Vec<PolicyDecision>,
    /// Hex SHA-256 of the normalized invocation arguments.
    pub arguments_hash: String,
    /// Remaining budget counters keyed by bucket.
    pub budget_remaining: BTreeMap<String, u64>,
    /// RFC 3339 time at which the step occurred / was observed.
    pub timestamp: String,
    /// Token issuer (SHOULD equal `verifier_id`).
    pub iss: String,
    /// JWT NumericDate issuance time.
    pub iat: u64,
    /// JWT NumericDate expiration time.
    pub exp: u64,
    /// Unique JWT id for replay detection.
    pub jti: String,

    // --- Optional MIC-Evidence / denial claims (omit key when unset) ---
    /// Coarse user-facing denial reason. MUST be absent for `compliant`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub public_denial_reason: Option<PublicDenialReason>,
    /// Audit-only denial code. MUST be absent for `compliant`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub internal_denial_code: Option<String>,
}
