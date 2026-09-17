//! ardur-observed-events — typed `ObservedEvent` emission for **local** tool steps.
//!
//! # Why this exists
//!
//! Gap G13 (finding A1 on ArdurAI/ardur-agent#502): the Ardur governance proxy
//! fronts *model* traffic. Local tool steps — `file.write`, `shell.run` — never
//! transit it, because they execute inside this process. Without an emitter the
//! plane can only govern model rounds, while the tools that actually mutate the
//! world stay outside it, and "every action is governed" would be false.
//!
//! This crate projects one tool execution into a verifier-contract v0.1 §6.2
//! `ObservedEvent` and applies the §9 fail-closed rules locally, so a runtime can
//! report each local step to the plane and honour the answer.
//!
//! # The honesty contract
//!
//! The verifier's verdict codomain is **three**-valued, and the third value is
//! the point:
//!
//! - [`Verdict::Compliant`] — every §6.3 requirement was established.
//! - [`Verdict::Violation`] — a rule was broken (revocation, budget, integrity).
//! - [`Verdict::InsufficientEvidence`] — *we could not see enough to judge*.
//!
//! "I could not observe it" must never render as "allowed". That is why
//! [`ObservedEvent::local_verdict`] returns `InsufficientEvidence` for missing
//! telemetry or a hidden hop rather than defaulting to compliant, and why
//! [`Visibility`] is carried explicitly instead of assumed `Full`.
//!
//! Per §9.2 the emitter **must not synthesize placeholder values** to rescue a
//! compliant verdict. Every constructor here therefore takes the real value or
//! records its absence; there is no `unwrap_or_default()` on a telemetry field.
//!
//! # Posture (decision A1)
//!
//! [`EnforcementPosture`] encodes the working default: synchronous `enforce` for
//! side-effecting classes, asynchronous `attest` for observe-class reads. The
//! emitter runs under *every* posture — only the blocking behaviour changes, so
//! a runtime that chooses attest-everything still produces the same evidence.
//!
//! # Scope
//!
//! This crate is deliberately standalone: it does not depend on the fused
//! runtime, so the projection and the fail-closed rules can be tested and
//! reviewed in isolation. Wiring it into stage 6 is a separate change against
//! files another workstream currently owns.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The verifier's three-valued verdict (§5).
///
/// Deliberately not `bool`: the whole point of the contract is that "could not
/// observe" is a distinct answer from "allowed" and from "denied".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every §6.3 requirement was established.
    Compliant,
    /// A normative rule was broken.
    Violation,
    /// The evidence was not sufficient to decide. Never treat as allowed.
    InsufficientEvidence,
}

impl Verdict {
    /// Whether a step carrying this verdict may proceed under `enforce`.
    ///
    /// Only [`Compliant`](Verdict::Compliant) permits execution. Both
    /// `Violation` and `InsufficientEvidence` block, which is the fail-closed
    /// rule: an unobservable step is not a permitted step.
    #[must_use]
    pub fn permits_execution(self) -> bool {
        matches!(self, Verdict::Compliant)
    }
}

/// Audit-only internal reason codes (§9 table).
///
/// These are *internal*: §10.1 requires user-facing denials to carry the minimal
/// public reason instead, because denial strings are an information-leakage
/// surface. Use [`Verdict`] for what the caller sees and this for the audit log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditCode {
    /// Required telemetry absent, or a hidden hop (§9.1, §9.2).
    TelemetryMissing,
    /// The grant or an ancestor was revoked (§9.3).
    Revoked,
    /// Budget over-reserve or overspend (§9.4).
    BudgetExhausted,
    /// Invocation envelope failed signature verification (§9.5).
    EnvelopeTampered,
    /// Observed manifest digest differs from the declared one (§9.6).
    ManifestDrift,
}

impl AuditCode {
    /// The public denial reason for this code (§9 table, §10.1).
    ///
    /// Several distinct internal codes collapse onto one public string on
    /// purpose — that narrowing is the information-leakage defence, so callers
    /// must surface *this*, never the [`AuditCode`].
    #[must_use]
    pub fn public_reason(self) -> &'static str {
        match self {
            AuditCode::TelemetryMissing => "insufficient_evidence",
            AuditCode::Revoked => "revoked",
            AuditCode::BudgetExhausted => "budget_exhausted",
            AuditCode::EnvelopeTampered | AuditCode::ManifestDrift => "policy_denied",
        }
    }
}

/// How much of a step the emitter could actually see (§6.2 `visibility`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Every declared field was observed.
    Full,
    /// Some fields were redacted or truncated.
    #[default]
    Partial,
    /// The step is known to have happened but was not observed.
    Hidden,
}

/// The side-effect class of a step, before budget normalization (§6.2).
///
/// This drives the A1 posture split: side-effecting classes are evaluated
/// synchronously because letting them run first would be unrecoverable, while
/// observe-class reads can be attested asynchronously.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectClass {
    /// No side effect: a read or a query.
    None,
    /// Writes local state (e.g. `file.write`).
    InternalWrite,
    /// Sends data outside the trust boundary (e.g. an HTTP POST).
    ExternalSend,
    /// Changes durable configuration or control state.
    StateChange,
}

impl SideEffectClass {
    /// Whether this class must be evaluated **before** the step runs.
    ///
    /// True for everything that mutates or egresses: once a write has landed or
    /// a request has left the process, a later "violation" verdict cannot undo
    /// it. Observe-class reads are safe to attest after the fact.
    #[must_use]
    pub fn requires_sync_enforcement(self) -> bool {
        !matches!(self, SideEffectClass::None)
    }
}

/// The local-step governance posture (decision A1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EnforcementPosture {
    /// Sync `enforce` for side-effecting classes, async `attest` for
    /// observe-class. The recommended working default.
    #[default]
    SyncSideEffectsAttestReads,
    /// Evaluate every step synchronously before it runs.
    SyncEverything,
    /// Attest everything after the fact; never block a local step.
    AttestEverything,
}

impl EnforcementPosture {
    /// Whether a step of `class` blocks on the plane's verdict under this
    /// posture.
    #[must_use]
    pub fn blocks(self, class: SideEffectClass) -> bool {
        match self {
            EnforcementPosture::SyncEverything => true,
            EnforcementPosture::AttestEverything => false,
            EnforcementPosture::SyncSideEffectsAttestReads => class.requires_sync_enforcement(),
        }
    }
}

/// A claimed budget effect (§6.2 `budget_delta`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetDelta {
    /// The effect-class bucket this delta applies to.
    pub effect_class: String,
    /// The claimed amount in that bucket's units.
    pub amount: u64,
}

/// The delegation edge a step belongs to, when it has one (§6.2).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationEdge {
    /// Parent grant or principal.
    pub delegation_from: Option<String>,
    /// Child grant or principal.
    pub delegation_to: Option<String>,
    /// Immediate parent event, if one exists.
    pub parent_event_id: Option<String>,
    /// Linked parent ER, when available (MIC-Evidence).
    pub parent_receipt_id: Option<String>,
    /// Linked downstream ERs if this step delegated (MIC-Evidence).
    #[serde(default)]
    pub downstream_receipt_ids: Vec<String>,
}

impl DelegationEdge {
    /// Whether this edge describes a **hidden hop** (§9.1).
    ///
    /// A hop is hidden when a step is known to be a child (it names a parent
    /// grant, or claims downstream receipts) but cannot be linked back to its
    /// parent edge — no `parent_event_id` and no `parent_receipt_id`. Such a
    /// step must yield `insufficient_evidence`, never `compliant`.
    #[must_use]
    pub fn is_hidden_hop(&self) -> bool {
        // Only an INCOMING delegation needs a parent link. `downstream_receipt_ids`
        // records children this event spawned — a root event that delegates
        // outward legitimately has no parent, and counting it here would make
        // every root delegation `insufficient_evidence`.
        let claims_incoming_delegation = self.delegation_from.is_some();
        let linkable = self.parent_event_id.is_some() || self.parent_receipt_id.is_some();
        claims_incoming_delegation && !linkable
    }
}

/// One observed local tool step, in the §6.2 canonical field set.
///
/// Field names match the spec exactly so the serialized form is portable; a
/// renamed field would silently fail conformance at the plane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedEvent {
    /// Stable identifier for this event.
    pub event_id: String,
    /// Session or trace identifier.
    pub session_id: String,
    /// RFC 3339 observation time.
    pub timestamp: String,
    /// Observed principal executing the step.
    pub actor: String,
    /// Governing delegation-grant identifier.
    pub grant_id: String,
    /// Tool or capability invoked.
    pub tool_name: String,
    /// High-level action family.
    pub action_class: String,
    /// Normalized target.
    pub target: String,
    /// Resource namespace used for policy matching.
    pub resource_family: String,
    /// Observed side-effect class before budget normalization.
    pub side_effect_class: SideEffectClass,
    /// Claimed budget effect.
    pub budget_delta: BudgetDelta,
    /// How much of the step was observable.
    pub visibility: Visibility,
    /// Delegation linkage for this step.
    #[serde(flatten)]
    pub delegation: DelegationEdge,
    /// Content category touched by the step.
    pub content_class: String,
    /// Sensitivity tier of the content or target.
    pub sensitivity: String,
    /// Whether observed content materially instructed the step.
    pub instruction_bearing: bool,
    /// Sanitized summary of the observed action.
    pub summary: String,
    /// Result of invocation-envelope verification.
    pub envelope_signature_valid: bool,
    /// Digest of the runtime tool-manifest snapshot.
    pub observed_manifest_digest: String,
}

/// Everything the local rules need that is not part of the event itself.
#[derive(Clone, Debug, Default)]
pub struct LineageContext {
    /// Declared manifest digest from the Mission Declaration.
    pub declared_manifest_digest: String,
    /// Telemetry fields the MD requires (§9.2).
    pub required_telemetry: Vec<String>,
    /// Whether this grant, or an ancestor, is revoked (§9.3).
    pub revoked: bool,
    /// Remaining budget per effect-class bucket (§9.4).
    pub remaining_budget: BTreeMap<String, u64>,
}

impl ObservedEvent {
    /// Apply the §9 fail-closed rules locally.
    ///
    /// Evaluation order matters and follows the spec's severity: integrity
    /// failures and revocation are `violation` regardless of observability,
    /// while telemetry gaps are `insufficient_evidence`. Returning
    /// `insufficient_evidence` for a *visible* budget overspend would understate
    /// a real breach, and returning `violation` for an unobservable step would
    /// overstate what we know — so neither is collapsed into the other.
    ///
    /// The `Ok` case is always [`Verdict::Compliant`]; every failure carries its
    /// audit code so the caller can log internally while surfacing only
    /// [`AuditCode::public_reason`].
    pub fn local_verdict(&self, ctx: &LineageContext) -> Result<Verdict, (Verdict, AuditCode)> {
        // §9.3 — cascading revocation. Checked first: a revoked lineage
        // invalidates the step whatever else is true of it.
        if ctx.revoked {
            return Err((Verdict::Violation, AuditCode::Revoked));
        }

        // §9.5 — envelope integrity. An integrity failure, explicitly *not* a
        // telemetry-quality failure.
        if !self.envelope_signature_valid {
            return Err((Verdict::Violation, AuditCode::EnvelopeTampered));
        }

        // §9.6 — manifest drift. Applies even when the substituted tool looks
        // policy-safe.
        //
        // An ABSENT declared digest is not a pass. Skipping the check when the
        // MD was unavailable (or a caller took a derived default) would let an
        // event with a valid envelope and full visibility read as `compliant`
        // while nothing at all pinned the registry — the exact substitution
        // §9.6 exists to catch. Not knowing what was declared is missing
        // evidence, so it is reported as such.
        if ctx.declared_manifest_digest.trim().is_empty() {
            return Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing));
        }
        if self.observed_manifest_digest != ctx.declared_manifest_digest {
            return Err((Verdict::Violation, AuditCode::ManifestDrift));
        }

        // §9.4 — a KNOWN budget overspend is checked BEFORE any telemetry gap.
        // Ordering matters: an event with a proven overspend AND partial
        // visibility would otherwise return `insufficient_evidence`, which
        // understates a rule we can actually prove was broken. Missing evidence
        // never outranks established fact.
        if let Some(code) = self.known_budget_violation(ctx) {
            return Err((Verdict::Violation, code));
        }

        // §9.1 — hidden hop.
        if self.delegation.is_hidden_hop() {
            return Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing));
        }

        // §6.4 — partial or hidden visibility cannot yield compliant under
        // portable v0.1 conformance, which assumes no stronger local model.
        if self.visibility != Visibility::Full {
            return Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing));
        }

        // §9.2 — every field named by the MD must be present AND usable.
        for field in &ctx.required_telemetry {
            if !self.has_usable_telemetry(field) {
                return Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing));
            }
        }

        // §9.4 — an UNTRACKED budget bucket is a telemetry gap, not a free
        // pass, so it fails closed rather than silently permitting an
        // unbudgeted effect. (The proven-overspend case is handled earlier, so
        // a known violation is never masked by a later evidence gap.)
        if self.budget_delta.amount > 0
            && !ctx
                .remaining_budget
                .contains_key(&self.budget_delta.effect_class)
        {
            return Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing));
        }

        Ok(Verdict::Compliant)
    }

    /// Whether a required telemetry field is present **and usable**.
    ///
    /// §9.2 counts a structurally-invalid or unusable value as missing, so an
    /// empty string fails exactly like an absent field: a blank `target` cannot
    /// be matched against a policy, and treating it as present would let a
    /// placeholder rescue a compliant verdict.
    /// A budget violation this event PROVES, independent of telemetry quality.
    ///
    /// Returns `Some` only when the bucket is explicitly tracked and the claimed
    /// spend exceeds what remains. An untracked bucket proves nothing and is
    /// handled as an evidence gap later.
    fn known_budget_violation(&self, ctx: &LineageContext) -> Option<AuditCode> {
        if self.budget_delta.amount == 0 {
            return None;
        }
        match ctx.remaining_budget.get(&self.budget_delta.effect_class) {
            Some(remaining) if self.budget_delta.amount > *remaining => {
                Some(AuditCode::BudgetExhausted)
            }
            _ => None,
        }
    }

    fn has_usable_telemetry(&self, field: &str) -> bool {
        match field {
            "event_id" => !self.event_id.trim().is_empty(),
            "session_id" => !self.session_id.trim().is_empty(),
            "timestamp" => !self.timestamp.trim().is_empty(),
            "actor" => !self.actor.trim().is_empty(),
            "grant_id" => !self.grant_id.trim().is_empty(),
            "tool_name" => !self.tool_name.trim().is_empty(),
            "action_class" => !self.action_class.trim().is_empty(),
            "target" => !self.target.trim().is_empty(),
            "resource_family" => !self.resource_family.trim().is_empty(),
            "summary" => !self.summary.trim().is_empty(),
            "content_class" => !self.content_class.trim().is_empty(),
            "sensitivity" => !self.sensitivity.trim().is_empty(),
            "observed_manifest_digest" => !self.observed_manifest_digest.trim().is_empty(),
            "budget_delta" => !self.budget_delta.effect_class.trim().is_empty(),
            // Every remaining canonical §6.2 field. These are typed (enums,
            // bools, option-linked ids) rather than free strings, so "usable"
            // means the value is carried at all — reporting them missing while
            // they sit in the serialized event would make an MD that names any
            // of them permanently unsatisfiable.
            "side_effect_class"
            | "visibility"
            | "instruction_bearing"
            | "envelope_signature_valid"
            | "content_provenance"
            | "confidence_hint" => true,
            "parent_event_id" => self.delegation.parent_event_id.is_some(),
            "delegation_from" => self.delegation.delegation_from.is_some(),
            "delegation_to" => !self.delegation.downstream_receipt_ids.is_empty(),
            // An unknown required field cannot be established, so it fails
            // closed rather than being ignored as "not my field".
            _ => false,
        }
    }
}

/// Builder for a §6.2 event from a tool execution.
///
/// Every telemetry field must be supplied explicitly. There is no
/// `Default`-filled constructor on purpose: §9.2 forbids synthesizing
/// placeholders to rescue a compliant verdict, and a builder that defaulted
/// `target` to `""` would do exactly that.
#[derive(Clone, Debug)]
pub struct ObservedEventBuilder {
    session_id: String,
    actor: String,
    grant_id: String,
    tool_name: String,
    action_class: String,
    target: String,
    resource_family: String,
    side_effect_class: SideEffectClass,
    budget_delta: BudgetDelta,
    visibility: Visibility,
    delegation: DelegationEdge,
    content_class: String,
    sensitivity: String,
    instruction_bearing: bool,
    summary: String,
    envelope_signature_valid: bool,
    observed_manifest_digest: String,
}

impl ObservedEventBuilder {
    /// Start an event for one tool step.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: impl Into<String>,
        actor: impl Into<String>,
        grant_id: impl Into<String>,
        tool_name: impl Into<String>,
        action_class: impl Into<String>,
        target: impl Into<String>,
        resource_family: impl Into<String>,
        side_effect_class: SideEffectClass,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            actor: actor.into(),
            grant_id: grant_id.into(),
            tool_name: tool_name.into(),
            action_class: action_class.into(),
            target: target.into(),
            resource_family: resource_family.into(),
            side_effect_class,
            budget_delta: BudgetDelta {
                effect_class: String::new(),
                amount: 0,
            },
            // Partial until the caller asserts otherwise: claiming full
            // visibility by default is exactly the dishonesty §6.4 guards
            // against.
            visibility: Visibility::Partial,
            delegation: DelegationEdge::default(),
            content_class: String::new(),
            sensitivity: String::new(),
            instruction_bearing: false,
            summary: String::new(),
            // False until verified, so an unverified envelope cannot pass.
            envelope_signature_valid: false,
            observed_manifest_digest: String::new(),
        }
    }

    /// Record the claimed budget effect.
    #[must_use]
    pub fn budget(mut self, effect_class: impl Into<String>, amount: u64) -> Self {
        self.budget_delta = BudgetDelta {
            effect_class: effect_class.into(),
            amount,
        };
        self
    }

    /// Assert how much of the step was observable.
    #[must_use]
    pub fn visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// Attach delegation linkage.
    #[must_use]
    pub fn delegation(mut self, delegation: DelegationEdge) -> Self {
        self.delegation = delegation;
        self
    }

    /// Record content classification.
    #[must_use]
    pub fn content(
        mut self,
        content_class: impl Into<String>,
        sensitivity: impl Into<String>,
        instruction_bearing: bool,
    ) -> Self {
        self.content_class = content_class.into();
        self.sensitivity = sensitivity.into();
        self.instruction_bearing = instruction_bearing;
        self
    }

    /// Record the sanitized summary.
    #[must_use]
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    /// Record the envelope-verification result and the observed manifest digest.
    #[must_use]
    pub fn envelope(
        mut self,
        signature_valid: bool,
        observed_manifest_digest: impl Into<String>,
    ) -> Self {
        self.envelope_signature_valid = signature_valid;
        self.observed_manifest_digest = observed_manifest_digest.into();
        self
    }

    /// Finish the event, stamping a fresh id and the current time.
    #[must_use]
    pub fn build(self) -> ObservedEvent {
        self.build_at(now_unix_millis(), uuid::Uuid::new_v4().to_string())
    }

    /// Finish the event with an explicit clock and id, for deterministic tests.
    #[must_use]
    pub fn build_at(self, unix_millis: u64, event_id: String) -> ObservedEvent {
        let event = ObservedEvent {
            event_id,
            session_id: self.session_id,
            timestamp: rfc3339_from_unix_millis(unix_millis),
            actor: self.actor,
            grant_id: self.grant_id,
            tool_name: self.tool_name,
            action_class: self.action_class,
            target: self.target,
            resource_family: self.resource_family,
            side_effect_class: self.side_effect_class,
            budget_delta: self.budget_delta,
            visibility: self.visibility,
            delegation: self.delegation,
            content_class: self.content_class,
            sensitivity: self.sensitivity,
            instruction_bearing: self.instruction_bearing,
            summary: self.summary,
            envelope_signature_valid: self.envelope_signature_valid,
            observed_manifest_digest: self.observed_manifest_digest,
        };
        if event.visibility != Visibility::Full {
            tracing::warn!(
                event_id = %event.event_id,
                tool = %event.tool_name,
                visibility = ?event.visibility,
                "emitting an ObservedEvent with degraded visibility; it cannot be compliant"
            );
        }
        event
    }
}

/// Digest a tool-manifest snapshot for `observed_manifest_digest`.
///
/// Entries are sorted before hashing so the digest is order-independent: a
/// registry that enumerates its tools in a different order is the *same*
/// manifest, and must not read as drift (§9.6 would otherwise fire spuriously).
#[must_use]
pub fn manifest_digest(tool_ids: &[String]) -> String {
    let mut sorted: Vec<&String> = tool_ids.iter().collect();
    sorted.sort();
    sorted.dedup();
    let mut hasher = Sha256::new();
    for id in sorted {
        // LENGTH-PREFIX, not a separator. A NUL delimiter collides the moment an
        // identifier can contain one: ["file.read", "shell.run"] and the single
        // id "file.read\0shell.run" feed identical bytes to SHA-256, so a
        // substituted registry would hash to the declared manifest and §9.6
        // would never fire. Tool ids come from remote/skill registration, so
        // that content is not fully under this crate's control.
        hasher.update((id.len() as u64).to_be_bytes());
        hasher.update(id.as_bytes());
    }
    // sha2 0.11's output array does not implement `LowerHex`, so hex-encode
    // explicitly — the same idiom the governance crate's `to_hex` uses.
    let digest: [u8; 32] = hasher.finalize().into();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Milliseconds since the Unix epoch.
fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Format Unix milliseconds as an RFC 3339 UTC timestamp.
///
/// Implemented against `std` rather than pulling in a date crate for one call.
/// The civil-calendar arithmetic is exact (proleptic Gregorian, with the
/// 4/100/400 leap rules) and is unit-tested against known instants including a
/// leap day and a year boundary.
#[must_use]
pub fn rfc3339_from_unix_millis(unix_millis: u64) -> String {
    let secs = unix_millis / 1_000;
    let millis = unix_millis % 1_000;
    let days = secs / 86_400;
    let secs_of_day = secs % 86_400;
    let (hour, minute, second) = (
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    );

    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Convert days since 1970-01-01 into a civil (year, month, day).
///
/// Howard Hinnant's `civil_from_days`: shift the epoch to 0000-03-01 so leap
/// days land at the end of the era, making the arithmetic branch-free.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder() -> ObservedEventBuilder {
        ObservedEventBuilder::new(
            "sess-1",
            "cli://localhost",
            "grant-1",
            "file.write",
            "write",
            "/tmp/scratch/out.txt",
            "fs",
            SideEffectClass::InternalWrite,
        )
        .budget("tool_exec", 1)
        .visibility(Visibility::Full)
        .content("text", "low", false)
        .summary("wrote 12 bytes")
        .envelope(true, "digest-abc")
    }

    fn ctx() -> LineageContext {
        LineageContext {
            declared_manifest_digest: "digest-abc".into(),
            required_telemetry: vec!["tool_name".into(), "target".into(), "grant_id".into()],
            revoked: false,
            remaining_budget: BTreeMap::from([("tool_exec".to_string(), 10)]),
        }
    }

    #[test]
    fn a_fully_observed_permitted_step_is_compliant() {
        let event = builder().build();
        assert_eq!(event.local_verdict(&ctx()), Ok(Verdict::Compliant));
    }

    #[test]
    fn insufficient_evidence_is_not_permission() {
        // The core honesty property: "could not observe" must never read as
        // "allowed" at the gate.
        assert!(!Verdict::InsufficientEvidence.permits_execution());
        assert!(!Verdict::Violation.permits_execution());
        assert!(Verdict::Compliant.permits_execution());
    }

    #[test]
    fn a_hidden_hop_yields_insufficient_evidence_not_compliant() {
        // §9.1: a child step that cannot be linked to its parent edge.
        let event = builder()
            .delegation(DelegationEdge {
                delegation_from: Some("parent-grant".into()),
                delegation_to: Some("child-grant".into()),
                parent_event_id: None,
                parent_receipt_id: None,
                downstream_receipt_ids: vec![],
            })
            .build();
        assert_eq!(
            event.local_verdict(&ctx()),
            Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing))
        );
    }

    #[test]
    fn a_linked_delegation_edge_is_not_a_hidden_hop() {
        let event = builder()
            .delegation(DelegationEdge {
                delegation_from: Some("parent-grant".into()),
                delegation_to: Some("child-grant".into()),
                parent_event_id: Some("evt-parent".into()),
                parent_receipt_id: None,
                downstream_receipt_ids: vec![],
            })
            .build();
        assert_eq!(event.local_verdict(&ctx()), Ok(Verdict::Compliant));
    }

    #[test]
    fn partial_visibility_cannot_be_compliant() {
        // §6.4: portable v0.1 conformance assumes no stronger local model.
        for degraded in [Visibility::Partial, Visibility::Hidden] {
            let event = builder().visibility(degraded).build();
            assert_eq!(
                event.local_verdict(&ctx()),
                Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing)),
                "{degraded:?} must not be compliant"
            );
        }
    }

    #[test]
    fn a_blank_required_field_counts_as_missing() {
        // §9.2: structurally invalid or unusable is missing. A blank target
        // cannot be matched against a policy.
        let event = ObservedEventBuilder::new(
            "sess-1",
            "cli://localhost",
            "grant-1",
            "file.write",
            "write",
            "   ",
            "fs",
            SideEffectClass::InternalWrite,
        )
        .budget("tool_exec", 1)
        .visibility(Visibility::Full)
        .envelope(true, "digest-abc")
        .build();
        assert_eq!(
            event.local_verdict(&ctx()),
            Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing))
        );
    }

    #[test]
    fn an_unknown_required_telemetry_field_fails_closed() {
        let mut ctx = ctx();
        ctx.required_telemetry.push("a_field_we_do_not_emit".into());
        let event = builder().build();
        assert_eq!(
            event.local_verdict(&ctx),
            Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing))
        );
    }

    #[test]
    fn revocation_is_a_violation_not_a_telemetry_gap() {
        let mut ctx = ctx();
        ctx.revoked = true;
        let event = builder().build();
        assert_eq!(
            event.local_verdict(&ctx),
            Err((Verdict::Violation, AuditCode::Revoked))
        );
    }

    #[test]
    fn revocation_outranks_an_unobservable_step() {
        // A revoked lineage is a violation even when visibility is degraded:
        // downgrading it to insufficient_evidence would understate a breach.
        let mut ctx = ctx();
        ctx.revoked = true;
        let event = builder().visibility(Visibility::Hidden).build();
        assert_eq!(
            event.local_verdict(&ctx),
            Err((Verdict::Violation, AuditCode::Revoked))
        );
    }

    #[test]
    fn an_invalid_envelope_is_an_integrity_violation() {
        // §9.5 is explicit that this is NOT a telemetry-quality failure.
        let event = builder().envelope(false, "digest-abc").build();
        assert_eq!(
            event.local_verdict(&ctx()),
            Err((Verdict::Violation, AuditCode::EnvelopeTampered))
        );
    }

    #[test]
    fn manifest_drift_is_a_violation_even_if_the_tool_looks_safe() {
        let event = builder().envelope(true, "some-other-digest").build();
        assert_eq!(
            event.local_verdict(&ctx()),
            Err((Verdict::Violation, AuditCode::ManifestDrift))
        );
    }

    #[test]
    fn overspend_is_a_violation() {
        let event = builder().budget("tool_exec", 999).build();
        assert_eq!(
            event.local_verdict(&ctx()),
            Err((Verdict::Violation, AuditCode::BudgetExhausted))
        );
    }

    #[test]
    fn an_untracked_budget_bucket_is_not_a_free_pass() {
        // An effect class the ledger does not track cannot be applied, so §6.3
        // item 4 is unsatisfiable: fail closed rather than permitting an
        // unbudgeted effect.
        let event = builder().budget("egress", 1).build();
        assert_eq!(
            event.local_verdict(&ctx()),
            Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing))
        );
    }

    #[test]
    fn public_reasons_narrow_the_internal_codes() {
        // §10.1: distinct internal codes deliberately collapse onto one public
        // string, so the denial does not leak which check fired.
        assert_eq!(AuditCode::EnvelopeTampered.public_reason(), "policy_denied");
        assert_eq!(AuditCode::ManifestDrift.public_reason(), "policy_denied");
        assert_eq!(
            AuditCode::TelemetryMissing.public_reason(),
            "insufficient_evidence"
        );
        assert_eq!(AuditCode::Revoked.public_reason(), "revoked");
        assert_eq!(
            AuditCode::BudgetExhausted.public_reason(),
            "budget_exhausted"
        );
    }

    #[test]
    fn posture_a1_blocks_side_effects_and_attests_reads() {
        let a1 = EnforcementPosture::SyncSideEffectsAttestReads;
        assert!(a1.blocks(SideEffectClass::InternalWrite));
        assert!(a1.blocks(SideEffectClass::ExternalSend));
        assert!(a1.blocks(SideEffectClass::StateChange));
        assert!(!a1.blocks(SideEffectClass::None));
    }

    #[test]
    fn the_other_postures_are_uniform() {
        for class in [
            SideEffectClass::None,
            SideEffectClass::InternalWrite,
            SideEffectClass::ExternalSend,
            SideEffectClass::StateChange,
        ] {
            assert!(EnforcementPosture::SyncEverything.blocks(class));
            assert!(!EnforcementPosture::AttestEverything.blocks(class));
        }
    }

    #[test]
    fn the_emitter_never_defaults_to_full_visibility_or_a_valid_envelope() {
        // A builder that assumed the happy path would let an unverified step
        // pass; both must be asserted explicitly by the caller.
        let event =
            ObservedEventBuilder::new("s", "a", "g", "t", "ac", "tgt", "rf", SideEffectClass::None)
                .build();
        assert_eq!(event.visibility, Visibility::Partial);
        assert!(!event.envelope_signature_valid);
    }

    #[test]
    fn manifest_digest_is_order_independent_but_content_sensitive() {
        let a = manifest_digest(&["file.read".into(), "shell.run".into()]);
        let b = manifest_digest(&["shell.run".into(), "file.read".into()]);
        assert_eq!(a, b, "tool order must not read as manifest drift");

        let c = manifest_digest(&["file.read".into()]);
        assert_ne!(a, c, "a different tool set must change the digest");
    }

    #[test]
    fn manifest_digest_separator_prevents_concatenation_collisions() {
        let a = manifest_digest(&["ab".into(), "c".into()]);
        let b = manifest_digest(&["a".into(), "bc".into()]);
        assert_ne!(a, b, "entry boundaries must be part of the digest");
    }

    #[test]
    fn rfc3339_formats_known_instants() {
        assert_eq!(rfc3339_from_unix_millis(0), "1970-01-01T00:00:00.000Z");
        // 2026-09-17T05:10:26.257Z — a date from this work's own logs.
        assert_eq!(
            rfc3339_from_unix_millis(1_789_621_826_257),
            "2026-09-17T05:10:26.257Z"
        );
        // Leap day.
        assert_eq!(
            rfc3339_from_unix_millis(1_709_164_800_000),
            "2024-02-29T00:00:00.000Z"
        );
        // Year boundary.
        assert_eq!(
            rfc3339_from_unix_millis(1_735_689_599_999),
            "2024-12-31T23:59:59.999Z"
        );
    }

    #[test]
    fn the_serialized_form_uses_the_spec_field_names() {
        // A renamed field would silently fail conformance at the plane, so the
        // wire form is pinned.
        let json = serde_json::to_value(builder().build()).expect("serializes");
        for field in [
            "event_id",
            "session_id",
            "timestamp",
            "actor",
            "grant_id",
            "tool_name",
            "action_class",
            "target",
            "resource_family",
            "side_effect_class",
            "budget_delta",
            "visibility",
            "content_class",
            "sensitivity",
            "instruction_bearing",
            "summary",
            "envelope_signature_valid",
            "observed_manifest_digest",
            // Flattened from DelegationEdge.
            "parent_event_id",
            "delegation_from",
            "delegation_to",
            "downstream_receipt_ids",
        ] {
            assert!(
                json.get(field).is_some(),
                "§6.2 field `{field}` missing from the serialized event"
            );
        }
        assert_eq!(json["side_effect_class"], "internal_write");
        assert_eq!(json["visibility"], "full");
    }

    #[test]
    fn events_round_trip_through_json() {
        let event = builder().build();
        let json = serde_json::to_string(&event).expect("serializes");
        let back: ObservedEvent = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(event, back);
    }

    #[test]
    fn an_absent_declared_manifest_digest_is_missing_evidence_not_a_pass() {
        // Skipping §9.6 when the MD was unavailable would let a fully-visible,
        // validly-enveloped event read as compliant with NOTHING pinning the
        // registry — the substitution the check exists to catch.
        let event = builder().build();
        let mut c = ctx();
        c.declared_manifest_digest = String::new();
        assert_eq!(
            event.local_verdict(&c),
            Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing))
        );

        c.declared_manifest_digest = "   ".into();
        assert_eq!(
            event.local_verdict(&c),
            Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing)),
            "whitespace is not a declared digest either"
        );
    }

    #[test]
    fn manifest_entries_are_length_prefixed_so_a_nul_cannot_forge_a_digest() {
        // With a NUL separator these two hash identically, so a substituted
        // registry would match the declared manifest and §9.6 never fires.
        let two = manifest_digest(&["file.read".to_string(), "shell.run".to_string()]);
        let one = manifest_digest(&["file.read\u{0}shell.run".to_string()]);
        assert_ne!(
            two, one,
            "a NUL inside a tool id must not collide with a two-entry manifest"
        );
    }

    #[test]
    fn an_outgoing_delegation_is_not_a_hidden_incoming_hop() {
        // A root event that spawns children legitimately has no parent link;
        // counting it as a hidden hop makes every root delegation
        // insufficient_evidence.
        let event = builder()
            .delegation(DelegationEdge {
                delegation_from: None,
                delegation_to: Some("child-grant".into()),
                parent_event_id: None,
                parent_receipt_id: None,
                downstream_receipt_ids: vec!["child-receipt-1".into()],
            })
            .build();
        assert_eq!(
            event.local_verdict(&ctx()),
            Ok(Verdict::Compliant),
            "recording children must not read as a hidden incoming hop"
        );
    }

    #[test]
    fn a_proven_overspend_outranks_a_telemetry_gap() {
        // Missing evidence must never mask a rule we can prove was broken.
        let event = builder()
            .budget("tool_exec", 1_000)
            .visibility(Visibility::Partial)
            .build();
        assert_eq!(
            event.local_verdict(&ctx()),
            Err((Verdict::Violation, AuditCode::BudgetExhausted))
        );
    }

    #[test]
    fn an_untracked_budget_bucket_is_still_only_missing_evidence() {
        // The reordering must not turn an UNPROVABLE overspend into a violation.
        let event = builder().budget("network", 5).build();
        assert_eq!(
            event.local_verdict(&ctx()),
            Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing))
        );
    }

    #[test]
    fn canonical_typed_telemetry_fields_are_not_reported_missing() {
        // An MD naming any of these would otherwise be permanently
        // unsatisfiable even though the value is right there in the event.
        let event = builder().build();
        let mut c = ctx();
        c.required_telemetry = vec![
            "side_effect_class".into(),
            "visibility".into(),
            "instruction_bearing".into(),
            "envelope_signature_valid".into(),
        ];
        assert_eq!(event.local_verdict(&c), Ok(Verdict::Compliant));
    }

    #[test]
    fn a_delegation_field_required_but_absent_still_fails_closed() {
        let event = builder().build();
        let mut c = ctx();
        c.required_telemetry = vec!["delegation_from".into()];
        assert_eq!(
            event.local_verdict(&c),
            Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing)),
            "a required delegation field with no value must not pass"
        );
    }
}
