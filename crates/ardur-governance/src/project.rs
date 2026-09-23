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

/// The grant facts an ER binds to, reduced to the fields the projection
/// actually consumes — the form durable per-event evidence (#543) can carry
/// when no live [`VerifiedClaims`] exists (crash-replay of an evidence
/// journal projects from records, not from a re-verified token).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantFacts {
    /// The cap-token `token_id` (hyphenated UUIDv4; ER `grant_id`).
    pub grant_id: String,
    /// The verified subject (ER `actor`).
    pub actor: String,
    /// The verified remaining budget (legacy economic scalar).
    pub budget_remaining: u64,
}

impl From<&VerifiedClaims> for GrantFacts {
    fn from(claims: &VerifiedClaims) -> Self {
        Self {
            grant_id: claims.token_id.to_string(),
            actor: claims.subject.0.clone(),
            budget_remaining: claims.budget_remaining,
        }
    }
}

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

/// The single, exhaustive (public reason, internal code) mapping every
/// cap-token verification failure classifies under — shared by
/// [`AuthOutcome::from_cap_token_error`] (live classification) and the
/// replay-vocabulary drift guard, so the two can never drift apart.
///
/// The match is deliberately **exhaustive with no wildcard arm**: a new
/// [`CapTokenError`] variant must break this build and force an explicit
/// classification decision, and the author adding it must also extend the
/// replay vocabulary in `canonical_public_for_denial_code` and the drift
/// guard's case list (the guard asserts whatever this mapping emits replays
/// canonically).
fn cap_error_pair(err: &CapTokenError) -> (PublicDenialReason, &'static str) {
    match err {
        CapTokenError::Expired => (PublicDenialReason::PolicyDenied, "grant_expired"),
        CapTokenError::AudienceMismatch => (PublicDenialReason::PolicyDenied, "audience_mismatch"),
        CapTokenError::BudgetExhausted => (PublicDenialReason::BudgetExhausted, "budget_exhausted"),
        CapTokenError::ToolNotAllowed => (PublicDenialReason::PolicyDenied, "tool_not_allowed"),
        CapTokenError::Revoked => (PublicDenialReason::Revoked, "revoked"),
        CapTokenError::SignatureInvalid => (PublicDenialReason::ChainInvalid, "signature_invalid"),
        CapTokenError::Malformed(_) => (PublicDenialReason::ChainInvalid, "malformed_token"),
        // Proof-of-possession (#363). These are POLICY denials, not chain
        // failures: the token itself may be perfectly well-formed and
        // correctly signed — what failed is the presenter's binding to it.
        // Mapping them to `ChainInvalid` would misdirect an operator toward
        // the issuer when the real problem is the caller.
        //
        // Missing proof and wrong proof stay distinct internally, because
        // "the client never sent one" (a misconfiguration) and "the key does
        // not match" (a stolen token being presented) demand different
        // responses, even though both are `PolicyDenied` publicly.
        CapTokenError::PopRequired(_) => (PublicDenialReason::PolicyDenied, "pop_required"),
        CapTokenError::PopKeyMismatch { .. } => {
            (PublicDenialReason::PolicyDenied, "pop_key_mismatch")
        }
        CapTokenError::PopInvalid(_) => (PublicDenialReason::PolicyDenied, "pop_invalid"),
        // The caller handles this case as `insufficient_evidence` before
        // delegating; the arm keeps the match exhaustive without a wildcard.
        CapTokenError::UnprojectableAttenuation(_) => (
            PublicDenialReason::InsufficientEvidence,
            "unprojectable_attenuation",
        ),
    }
}

impl AuthOutcome {
    /// Map a cap-token verification failure to an ER outcome, following the
    /// verifier-contract §9 fail-closed table.
    ///
    /// The match is deliberately **exhaustive with no wildcard arm**. A new
    /// `CapTokenError` variant must break this build and force an explicit
    /// classification decision: a `_ =>` catch-all would silently file every
    /// future failure under whatever the fallback happened to be, which is how
    /// an unprojectable token could come to look like a clean policy denial.
    pub fn from_cap_token_error(err: &CapTokenError) -> Self {
        // Not every verification failure is a *violation*. When the verifier
        // cannot project an attenuation into effective claims it has not caught
        // the token breaking a rule — it has failed to establish what the token
        // authorizes at all. §9.2 calls a value that is "unusable for
        // deterministic policy evaluation" missing telemetry, so the honest
        // verdict is `insufficient_evidence`. Reporting it as a violation would
        // claim knowledge the verifier does not have.
        if let CapTokenError::UnprojectableAttenuation(statement) = err {
            // Log a DIGEST, never the statement. Attenuation literals are
            // holder-controlled and may carry paths, identifiers or credential
            // material; emitting them verbatim turns a verification warning into
            // a data-exfiltration channel that survives in log aggregation. The
            // digest still correlates repeat occurrences of the same offending
            // block across runs, which is what the operator actually needs.
            tracing::warn!(
                statement_digest = %sha256_hex(statement.as_bytes()),
                statement_len = statement.len(),
                "cap-token attenuation could not be projected; reporting insufficient_evidence"
            );
            return AuthOutcome::InsufficientEvidence {
                internal: "unprojectable_attenuation".to_string(),
            };
        }

        let (public, internal) = cap_error_pair(err);
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
    /// Remaining budget per effect-class bucket, keyed by the SHARED
    /// effect-bucket registry vocabulary (#545). When supplied, this is the
    /// `budget_remaining` the ER carries — validated through
    /// [`crate::effect_bucket_registry`], so a key outside the normative
    /// five-class namespace fails projection instead of reaching a verifier
    /// as an invented bucket.
    ///
    /// `None` (the legacy form) keeps the cap-token's single economic scalar
    /// under the `cost` key. That key is NOT a normative bucket: §5.4
    /// requires lineage budgets in the effect-class namespace, so a receipt
    /// with `cost` alone carries no MIC-State budget claim — it is an
    /// economic axis, per #545's "native cents/tokens remain separate
    /// economic axes" rule.
    pub per_class_budget_remaining: Option<&'a BTreeMap<String, u64>>,
}

/// The JCS-canonical digests of a normalized invocation: the hex SHA-256 of
/// the canonical arguments (ER `arguments_hash`) and the [`DigestObject`] over
/// the canonical invocation envelope (ER `invocation_digest`). This is the
/// SINGLE definition of the envelope shape — the live projector, the #543
/// evidence recorder, and the evidence replay projector all hash through it,
/// so a recorded digest and a recomputed one are comparable byte-for-byte.
pub fn invocation_digests(grant_id: &str, call: &ToolInvocation<'_>) -> (String, DigestObject) {
    // arguments_hash over the JCS-canonical normalized arguments.
    let arguments_hash = sha256_hex(&jcs::to_canonical_bytes(call.arguments));

    // invocation_digest over the JCS-canonical normalized invocation envelope.
    let envelope = json!({
        "action_class": serde_plain(&call.action_class),
        "arguments": call.arguments,
        "grant_id": grant_id,
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
    (arguments_hash, invocation_digest)
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
    let grant = GrantFacts::from(claims);
    let (arguments_hash, invocation_digest) = invocation_digests(&grant.grant_id, call);
    project_execution_receipt_core(
        &grant,
        call.tool,
        call.action_class,
        call.target,
        call.resource_family,
        call.side_effect_class,
        arguments_hash,
        invocation_digest,
        outcome,
        step,
        "cap-token",
    )
}

/// The projection core every path funnels through: the round mirror (#560,
/// digests computed from live facts) and the #543 per-event projector
/// (digests recomputed from — or, when the arguments exceeded the evidence
/// inline cap, replayed out of — the durable record). All schema validation
/// and the verdict invariant live here exactly once.
#[allow(clippy::too_many_arguments)]
pub fn project_execution_receipt_core(
    grant: &GrantFacts,
    tool: &str,
    action_class: ActionClass,
    target: &str,
    resource_family: &str,
    side_effect_class: SideEffectClass,
    arguments_hash: String,
    invocation_digest: DigestObject,
    outcome: &AuthOutcome,
    step: &StepContext,
    decision_backend: &str,
) -> Result<ExecutionReceipt, GovernanceError> {
    validate_id_string("trace_id", step.trace_id)?;
    validate_len("step_id", step.step_id, 1, 256)?; // nonEmptyString
    validate_run_nonce(step.run_nonce)?;
    validate_id_string("grant_id", &grant.grant_id)?;
    validate_len("target", target, 1, 2048)?;

    let timestamp = rfc3339_from_millis(step.timestamp_millis)?;
    let iat = step.timestamp_millis / 1000;
    let exp = iat + step.ttl_secs.max(1);

    let parent_receipt_hash = step.parent.map(|p| p.receipt_hash());
    let parent_receipt_id = parent_receipt_hash.as_ref().map(|h| h[..16].to_string());

    // Stable, collision-resistant receipt_id / jti (mirrors the reference impl:
    // a hash over the id-free step material).
    let stable_seed = format!(
        "{}|{}|{}|{}|{}",
        step.trace_id, step.run_nonce, step.step_id, grant.grant_id, invocation_digest.value
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

    let budget_remaining = match &step.per_class_budget_remaining {
        Some(per_class) => crate::effect::project_budget_remaining(
            per_class,
            &crate::effect::effect_bucket_registry(),
        )?,
        None => {
            // Legacy economic scalar: NOT a normative bucket (§5.4) — see
            // StepContext::per_class_budget_remaining. Retained because the
            // cap-token budget is one axis and inventing five buckets from
            // it would be the opposite dishonesty.
            BTreeMap::from([("cost".to_string(), grant.budget_remaining)])
        }
    };

    let policy_decisions = vec![PolicyDecision {
        backend: decision_backend.to_string(),
        decision,
        reason: Some(reason.clone()),
        eval_ms: None,
    }];

    let receipt = ExecutionReceipt {
        receipt_id,
        grant_id: grant.grant_id.clone(),
        parent_receipt_id,
        parent_receipt_hash,
        actor: grant.actor.clone(),
        verifier_id: step.verifier_id.to_string(),
        trace_id: step.trace_id.to_string(),
        run_nonce: step.run_nonce.to_string(),
        step_id: step.step_id.to_string(),
        invocation_digest,
        tool: tool.to_string(),
        action_class,
        target: target.to_string(),
        resource_family: resource_family.to_string(),
        side_effect_class,
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

/// The internal denial code when the observed effect's output was blocked at
/// the injection-defense re-admission scan. The tool's effect already
/// happened (and is digest-bound in the durable record); the *step* — tool
/// invocation plus output re-entry — ended in a policy denial.
pub const OUTPUT_SCAN_BLOCKED_CODE: &str = "output_scan_blocked";
/// The internal code when the scanner itself failed operationally (a filter
/// error, not a block verdict): output admission could not be determined, so
/// the honest verdict is `insufficient_evidence`, never a guessed violation.
pub const OUTPUT_SCAN_ERROR_CODE: &str = "output_scan_error";
/// The internal code when the invocation exceeded its deadline after a
/// possible dispatch: the effect is unknown and is never guessed.
pub const EFFECT_UNKNOWN_TIMEOUT_CODE: &str = "effect_unknown_timeout";
/// The internal code when the tool returned an execution error: a tool error
/// does not imply no external effect occurred, so the outcome is unknown.
pub const EFFECT_UNKNOWN_EXECUTION_CODE: &str = "effect_unknown_execution";
/// The internal code when a crash (or a dropped stream) stranded the event
/// between its durable pre-effect record and any observation: the tool may
/// have run; nothing was observed.
pub const EFFECT_UNOBSERVED_CODE: &str = "effect_unobserved";
/// The internal code when the durable record kept only the invocation
/// *digests* (arguments exceeded the evidence inline cap): an otherwise
/// compliant outcome cannot be shown compliant without the inputs.
pub const ARGUMENTS_EVIDENCE_OMITTED_CODE: &str = "arguments_evidence_omitted";

/// The shape of a `sha256_hex` output: 64 lowercase hex characters. Used to
/// validate recorded digests before they are replayed into a signed ER (the
/// digest-only replay path cannot recompute them for a cross-check).
fn is_lower_hex_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The shape of a base64url-no-pad encoded SHA-256, checked by actually
/// decoding: exactly 32 bytes, and canonically encoded (a 43-character
/// value whose final digit carries nonzero padding bits is a different
/// string for the same bytes and must fail closed, not be signed). Used to
/// validate recorded digests before they are replayed into a signed ER.
fn is_base64url_sha256(s: &str) -> bool {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
    let Ok(bytes) = B64URL.decode(s) else {
        return false;
    };
    bytes.len() == 32 && B64URL.encode(&bytes) == s
}

/// Project one **evaluated event** (#543) into an [`ExecutionReceipt`] from
/// its durable pre/post-effect evidence records — the same projection whether
/// the facts arrive live at the event's terminal point or are replayed out of
/// the evidence journal after a crash.
///
/// Replay-idempotent by construction: `step_id` is the stable event id, the
/// run nonce is derived deterministically from it, and the receipt id / jti
/// hash over id-free recorded material, so a live projection and a crash
/// replay of the same records mint the *same* ER claims (modulo the chain
/// link), never a duplicate.
///
/// # Fail-closed honesty rules
///
/// - `post` is `None` (a crash stranded the event pre-observation) →
///   `insufficient_evidence` ([`EFFECT_UNOBSERVED_CODE`]); the tool is never
///   re-executed to fill the gap.
/// - The record's canonical arguments are present → the digests are
///   **recomputed** and must equal the recorded ones; a mismatch means the
///   journal was tampered with and fails the projection (and, through the
///   emitter, the mirror open) instead of minting onto forged inputs.
/// - The record kept digests only (arguments over the inline cap) → the
///   recorded digests are replayed, but an otherwise-`compliant` outcome is
///   escalated to `insufficient_evidence`
///   ([`ARGUMENTS_EVIDENCE_OMITTED_CODE`]): compliance cannot be claimed for
///   inputs the evidence cannot show. Denials stay denials — the gate's
///   decision is itself the sufficient fact.
///
/// # Errors
///
/// [`GovernanceError::InvalidClaim`] on a schema-invalid recorded field, an
/// argument/digest mismatch (tampered journal), or a `post` record naming a
/// different event than `pre`.
pub fn project_event_execution_receipt(
    pre: &crate::evidence::PreEffectRecord,
    post: Option<&crate::evidence::PostEffectRecord>,
    verifier_id: &str,
    ttl_secs: u64,
    parent: Option<&SignedExecutionReceipt>,
) -> Result<ExecutionReceipt, GovernanceError> {
    use crate::evidence::{EventOutcome, EvidenceOutputAdmission};

    if let Some(post) = post {
        if post.event_id != pre.event_id {
            return Err(GovernanceError::InvalidClaim(format!(
                "post-effect record names event {} but the pre-effect record names {}",
                post.event_id, pre.event_id
            )));
        }
    }

    // The verdict and the observation timestamp the ER will carry.
    let (mut outcome, timestamp_millis) = match post {
        None => (
            AuthOutcome::InsufficientEvidence {
                internal: EFFECT_UNOBSERVED_CODE.to_string(),
            },
            pre.recorded_at_ms,
        ),
        Some(post) => {
            let outcome = match &post.outcome {
                EventOutcome::Completed(completed) => {
                    // The journal is untrusted at replay: a corrupt completed
                    // observation must fail closed at the open, not be hashed
                    // into a signed reason.
                    if !is_lower_hex_sha256(&completed.output_digest) {
                        return Err(GovernanceError::InvalidClaim(format!(
                            "evidence integrity: the recorded output digest for event {} is \
                             not a lowercase-hex SHA-256 (tampered or corrupt journal)",
                            pre.event_id
                        )));
                    }
                    match completed.output_admission {
                        EvidenceOutputAdmission::Allowed => AuthOutcome::Compliant,
                        EvidenceOutputAdmission::Blocked => AuthOutcome::Violation {
                            public: PublicDenialReason::PolicyDenied,
                            internal: OUTPUT_SCAN_BLOCKED_CODE.to_string(),
                        },
                        EvidenceOutputAdmission::Undetermined => {
                            AuthOutcome::InsufficientEvidence {
                                internal: OUTPUT_SCAN_ERROR_CODE.to_string(),
                            }
                        }
                    }
                }
                EventOutcome::Denied(denied) => {
                    // Replay trusts no (public, internal) pair it cannot
                    // account for: the code must be in the vocabulary and
                    // carry its canonical public reason, or the record is
                    // corrupt — never signed.
                    match canonical_public_for_denial_code(&denied.internal) {
                        Some(canonical) if canonical == denied.public => {}
                        _ => {
                            return Err(GovernanceError::InvalidClaim(format!(
                                "evidence integrity: the recorded denial code {:?} does not \
                                 pair with public reason {:?} (tampered or corrupt journal)",
                                denied.internal, denied.public
                            )));
                        }
                    }
                    match denied.public {
                        // A denial whose own classification is "could not
                        // establish what this authorizes" stays insufficient —
                        // filing it as a violation would claim knowledge the
                        // verifier does not have (§9.2).
                        PublicDenialReason::InsufficientEvidence => {
                            AuthOutcome::InsufficientEvidence {
                                internal: denied.internal.clone(),
                            }
                        }
                        _ => AuthOutcome::Violation {
                            public: denied.public,
                            internal: denied.internal.clone(),
                        },
                    }
                }
                EventOutcome::FailedUnknown => AuthOutcome::InsufficientEvidence {
                    internal: EFFECT_UNKNOWN_EXECUTION_CODE.to_string(),
                },
                EventOutcome::TimeoutUnknown => AuthOutcome::InsufficientEvidence {
                    internal: EFFECT_UNKNOWN_TIMEOUT_CODE.to_string(),
                },
            };
            (outcome, post.recorded_at_ms)
        }
    };

    // The invocation digests: recomputed from the recorded arguments when they
    // are inline (cross-checked against the record), replayed from the record
    // when the arguments exceeded the inline cap.
    let (arguments_hash, invocation_digest) = match &pre.arguments {
        Some(arguments) => {
            let call = pre.as_tool_invocation(arguments);
            let (recomputed_hash, recomputed_digest) = invocation_digests(&pre.grant_id, &call);
            if recomputed_hash != pre.arguments_hash
                || recomputed_digest.value != pre.invocation_digest
            {
                return Err(GovernanceError::InvalidClaim(format!(
                    "evidence integrity: the recorded arguments for event {} do not hash to \
                     the recorded digests (tampered or corrupt journal)",
                    pre.event_id
                )));
            }
            (recomputed_hash, recomputed_digest)
        }
        None => {
            // Replay copies the recorded digest strings into a signed ER;
            // validate their encodings first. A syntactically valid journal
            // line carrying a corrupted digest must fail closed here, exactly
            // as the inline-arguments branch fails on a hash mismatch —
            // otherwise a tampered journal stranded before its live mirror
            // would be reopened and signed.
            if !is_lower_hex_sha256(&pre.arguments_hash) {
                return Err(GovernanceError::InvalidClaim(format!(
                    "evidence integrity: the recorded arguments_hash for event {} is not a \
                     lowercase-hex SHA-256 (tampered or corrupt journal)",
                    pre.event_id
                )));
            }
            if !is_base64url_sha256(&pre.invocation_digest) {
                return Err(GovernanceError::InvalidClaim(format!(
                    "evidence integrity: the recorded invocation_digest for event {} is not a \
                     base64url SHA-256 (tampered or corrupt journal)",
                    pre.event_id
                )));
            }
            (
                pre.arguments_hash.clone(),
                DigestObject {
                    alg: DigestAlg::Sha256,
                    canonicalization: Some(Canonicalization::JcsRfc8785),
                    scope: Some(DigestScope::NormalizedInput),
                    value: pre.invocation_digest.clone(),
                },
            )
        }
    };

    // Missing reconstruction inputs never mint compliance.
    if pre.arguments.is_none() && matches!(outcome, AuthOutcome::Compliant) {
        outcome = AuthOutcome::InsufficientEvidence {
            internal: ARGUMENTS_EVIDENCE_OMITTED_CODE.to_string(),
        };
    }

    let run_nonce = crate::evidence::event_run_nonce(&pre.event_id);
    let grant = GrantFacts {
        grant_id: pre.grant_id.clone(),
        actor: pre.actor.clone(),
        budget_remaining: pre.budget_remaining,
    };
    let backend = decision_backend_for(&outcome);
    let mut receipt = project_execution_receipt_core(
        &grant,
        &pre.tool,
        pre.action_class,
        &pre.target,
        &pre.resource_family,
        pre.side_effect_class,
        arguments_hash,
        invocation_digest,
        &outcome,
        &StepContext {
            verifier_id,
            iss: verifier_id,
            trace_id: &pre.session_id,
            run_nonce: &run_nonce,
            step_id: &pre.event_id,
            timestamp_millis,
            ttl_secs,
            evidence_level: EvidenceLevel::SelfSigned,
            parent,
            // #543 keeps the Phase 1 economic scalar: per-effect-class budgets
            // remain #545-tracked and are not invented per event.
            per_class_budget_remaining: None,
        },
        backend,
    )?;
    // Bind the durable evidence into the signed claims: the ER's audit
    // `reason` carries the SHA-256 over the canonical pre-effect journal
    // line and — when the event reached a terminal observation — the
    // post-effect line too (the v0.1 schema's `additionalProperties: false`
    // leaves no free field for it). Without this, editing a chained event's
    // recorded provenance (kind, iteration, ordinal, call id) or outcome
    // would re-project an exactly equal receipt and pass the reopen
    // reconciliation unchecked.
    let mut evidence =
        crate::evidence::EvidenceRecord::PreEffect(Box::new(pre.clone())).to_line()?;
    if let Some(post) = post {
        evidence.push('\n');
        evidence.push_str(&crate::evidence::EvidenceRecord::PostEffect(post.clone()).to_line()?);
    }
    receipt.reason = format!(
        "{}; evidence sha256:{}",
        receipt.reason,
        sha256_hex(evidence.as_bytes())
    );
    Ok(receipt)
}

/// The canonical public denial reason an event's internal denial code pairs
/// with, or `None` for a code outside the #543 vocabulary. The journal is
/// explicitly untrusted at replay: a syntactically valid record pairing e.g.
/// `revoked` with `approval_rejected` must fail closed at the open, not be
/// signed with a backend attribution its fields disagree on.
fn canonical_public_for_denial_code(internal: &str) -> Option<PublicDenialReason> {
    Some(match internal {
        "grant_expired"
        | "tool_not_allowed"
        | "audience_mismatch"
        | "capability_not_granted"
        | "policy_denied"
        | "unknown_tool"
        | "approval_required"
        | "approval_rejected"
        | "output_scan_blocked"
        | "memory_capability_denied"
        | "memory_policy_denied"
        | "memory_subject_mismatch"
        | "memory_receipt_required"
        | "memory_record_malformed"
        | "memory_write_denied"
        | "pop_required"
        | "pop_key_mismatch"
        | "pop_invalid"
        | "tool_capability_denied"
        | "tool_invalid_arguments" => PublicDenialReason::PolicyDenied,
        "revoked" => PublicDenialReason::Revoked,
        "signature_invalid" | "malformed_token" => PublicDenialReason::ChainInvalid,
        "budget_exhausted" | "tool_cost_ceiling_exceeded" => PublicDenialReason::BudgetExhausted,
        "policy_indeterminate"
        | "memory_policy_indeterminate"
        | "approval_evaluation_error"
        | "tool_invocation_error"
        | "tool_not_implemented"
        | "unprojectable_attenuation" => PublicDenialReason::InsufficientEvidence,
        _ => return None,
    })
}

/// The policy backend an event outcome's verdict is attributed to (ER
/// `policy_decisions[].backend`) — the deciding gate, never the round
/// mirror's cap-token catch-all. An output-scanner block, a Cedar denial, an
/// approval rejection, or a memory-policy denial signed as a cap-token
/// decision would corrupt audit attribution, so the durable outcome's
/// internal code selects the backend; verdicts produced by the verifier's
/// own evidence rules (an unobserved or unknown effect, omitted argument
/// evidence, an operational failure that never reached a decision) attribute
/// to the verifier, not to a gate that never decided.
fn decision_backend_for(outcome: &AuthOutcome) -> &'static str {
    let internal = match outcome {
        AuthOutcome::Compliant => return "cap-token",
        AuthOutcome::Violation { internal, .. }
        | AuthOutcome::InsufficientEvidence { internal } => internal.as_str(),
    };
    match internal {
        "policy_denied"
        | "policy_indeterminate"
        | "memory_policy_denied"
        | "memory_policy_indeterminate" => "cedar",
        "approval_required" | "approval_rejected" | "approval_evaluation_error" => "approval",
        "output_scan_blocked" | "output_scan_error" => "injection-scanner",
        "memory_record_malformed" => "memory-control-plane",
        "unknown_tool" => "tool-registry",
        // Typed in-tool refusals (#543): the tool itself refused before the
        // effect (allowlist, root escape, missing grant, rejected token,
        // malformed arguments, cost ceiling, unimplemented backend).
        "tool_capability_denied"
        | "tool_invalid_arguments"
        | "tool_cost_ceiling_exceeded"
        | "tool_not_implemented" => "tool-runtime",
        "effect_unobserved"
        | "effect_unknown_execution"
        | "effect_unknown_timeout"
        | "arguments_evidence_omitted"
        | "tool_invocation_error" => "verifier",
        // grant_expired, tool_not_allowed, revoked, audience_mismatch,
        // signature_invalid, budget_exhausted, capability_not_granted,
        // memory_capability_denied, memory_receipt_required,
        // memory_subject_mismatch, memory_write_denied
        _ => "cap-token",
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift guard: every (public, internal) pair the shared exhaustive
    /// cap-error mapping emits must exist in the replay vocabulary. Without
    /// this a durably recorded denial would be rejected at replay —
    /// poisoning the emitter and every restart that replays the record. The
    /// mapping itself is exhaustive, so a new [`CapTokenError`] variant
    /// breaks THIS crate's build in `cap_error_pair` first; the author
    /// fixing it must extend the vocabulary and this case list (Rust enums
    /// are not enumerable at compile time, so the case list is the remaining
    /// manual step — the shared mapping guarantees the pairs themselves
    /// cannot drift from the live classification).
    #[test]
    fn every_cap_token_error_pair_is_in_the_replay_vocabulary() {
        let errors = vec![
            CapTokenError::Expired,
            CapTokenError::AudienceMismatch,
            CapTokenError::BudgetExhausted,
            CapTokenError::ToolNotAllowed,
            CapTokenError::Revoked,
            CapTokenError::SignatureInvalid,
            CapTokenError::Malformed("m".to_string()),
            CapTokenError::PopRequired("m".to_string()),
            CapTokenError::PopKeyMismatch {
                expected: "e".to_string(),
                presented: "p".to_string(),
            },
            CapTokenError::PopInvalid("m".to_string()),
            CapTokenError::UnprojectableAttenuation("m".to_string()),
        ];
        for err in errors {
            let (public, internal) = cap_error_pair(&err);
            assert_eq!(
                canonical_public_for_denial_code(internal),
                Some(public),
                "the pair {public:?}/{internal} from the shared mapping must replay canonically"
            );
            // And the live classification emits exactly the shared pair.
            match AuthOutcome::from_cap_token_error(&err) {
                AuthOutcome::Violation {
                    public: p,
                    internal: i,
                } => assert_eq!((p, i.as_str()), (public, internal)),
                AuthOutcome::InsufficientEvidence { internal: i } => {
                    assert_eq!(
                        (PublicDenialReason::InsufficientEvidence, i.as_str()),
                        (public, internal)
                    );
                }
                AuthOutcome::Compliant => panic!("a cap error never maps to compliant"),
            }
        }
    }
}
