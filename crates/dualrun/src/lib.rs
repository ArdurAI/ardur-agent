//! Shadow-mode dual-run verdict comparison (gh#502 B4).
//!
//! Two verification paths now exist: the legacy path that decides today, and
//! the AAT-profile path built for the governance convergence. Switching
//! directly from one to the other is not honest engineering — nobody knows
//! whether they agree.
//!
//! This crate runs them side by side in **shadow mode**: both verdicts are
//! computed, **legacy decides**, and disagreements are recorded as durable
//! evidence rather than log lines. Promotion to enforcing is a separate,
//! deliberate act gated on measured agreement.
//!
//! # Why shadow first
//!
//! "Fail closed on mismatch" sounds safer and is worse. If the new path is
//! stricter, every disagreement becomes a denial during rollout — an outage
//! caused by a verifier nobody has validated yet. If it is more permissive,
//! failing closed means falling back to legacy, so the new path never actually
//! gates anything and the exercise proves nothing. Shadow mode separates
//! *measuring* agreement from *acting* on it.
//!
//! # Why mismatches are receipts, not logs
//!
//! A mismatch is the entire output of this exercise. Logs rotate, are sampled,
//! and cannot be replayed; a promotion decision resting on "we didn't see many
//! in the logs" is unfalsifiable. Each mismatch is emitted as a
//! [`MISMATCH_VERB`] receipt carrying **both verdict digests**, so the
//! promotion gate is computed from the same append-only chain everything else
//! is audited against.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Receipt verb recorded when the two paths disagree.
pub const MISMATCH_VERB: &str = "governance.dualrun.mismatch.v1";

/// Governed steps that must agree before the new path may decide.
pub const PROMOTION_MIN_STEPS: u64 = 1000;

/// Seconds the comparison must run before promotion, regardless of volume.
///
/// Seven days. Volume alone is a weak gate: a thousand steps in one afternoon
/// exercises one workload on one machine. The time bound is what forces the
/// comparison across different days, operators, and provider conditions.
pub const PROMOTION_MIN_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Which verification path produced a verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Path {
    /// The path that decides today.
    Legacy,
    /// The AAT-profile path under evaluation.
    Aat,
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Path::Legacy => write!(f, "legacy"),
            Path::Aat => write!(f, "aat"),
        }
    }
}

/// A tri-state verdict, structurally shared by both paths.
///
/// Kept local rather than depending on either path's enum: this crate must be
/// able to compare them *because* they are separate types that may drift, and
/// importing one would quietly make that path's spelling authoritative.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Sufficient evidence; within policy.
    Compliant,
    /// Sufficient evidence; policy or integrity violation.
    Violation,
    /// Could not honestly determine compliance. Never treat as allowed.
    InsufficientEvidence,
}

impl Verdict {
    /// The canonical wire spelling, used for digests.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Compliant => "compliant",
            Verdict::Violation => "violation",
            Verdict::InsufficientEvidence => "insufficient_evidence",
        }
    }

    /// Whether this verdict permits execution. Only `Compliant` does.
    #[must_use]
    pub fn permits_execution(self) -> bool {
        matches!(self, Verdict::Compliant)
    }
}

/// One path's answer for one governed step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathVerdict {
    /// Which path produced it.
    pub path: Path,
    /// The verdict itself.
    pub verdict: Verdict,
    /// The path's internal reason code, when it produced one.
    pub audit_code: Option<String>,
}

/// A verdict from the path that decides.
///
/// Separate types rather than a runtime role check: `debug_assert!` compiles
/// out in release, so a caller that swapped the two arguments would silently
/// let the shadow path govern the action — precisely the invariant this crate
/// exists to hold. Distinct types make that swap a compile error instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyVerdict(PathVerdict);

/// A verdict from the path under evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AatVerdict(PathVerdict);

impl LegacyVerdict {
    /// Tag a verdict as the deciding path's.
    #[must_use]
    pub fn new(verdict: Verdict) -> Self {
        Self(PathVerdict::new(Path::Legacy, verdict))
    }

    /// Tag a verdict carrying the path's internal reason code.
    #[must_use]
    pub fn with_code(verdict: Verdict, audit_code: impl Into<String>) -> Self {
        Self(PathVerdict::with_code(Path::Legacy, verdict, audit_code))
    }

    /// The underlying verdict record.
    #[must_use]
    pub fn inner(&self) -> &PathVerdict {
        &self.0
    }
}

impl AatVerdict {
    /// Tag a verdict as the shadow path's.
    #[must_use]
    pub fn new(verdict: Verdict) -> Self {
        Self(PathVerdict::new(Path::Aat, verdict))
    }

    /// Tag a verdict carrying the path's internal reason code.
    #[must_use]
    pub fn with_code(verdict: Verdict, audit_code: impl Into<String>) -> Self {
        Self(PathVerdict::with_code(Path::Aat, verdict, audit_code))
    }

    /// The underlying verdict record.
    #[must_use]
    pub fn inner(&self) -> &PathVerdict {
        &self.0
    }
}

impl PathVerdict {
    /// A verdict with no reason code.
    #[must_use]
    pub fn new(path: Path, verdict: Verdict) -> Self {
        Self {
            path,
            verdict,
            audit_code: None,
        }
    }

    /// A verdict carrying the path's internal reason code.
    #[must_use]
    pub fn with_code(path: Path, verdict: Verdict, audit_code: impl Into<String>) -> Self {
        Self {
            path,
            verdict,
            audit_code: Some(audit_code.into()),
        }
    }

    /// A stable digest of this verdict.
    ///
    /// Length-prefixed rather than delimiter-joined: with a separator, a code
    /// containing that separator could make two different verdicts hash alike,
    /// and a mismatch record that cannot distinguish two verdicts is worthless
    /// as evidence.
    #[must_use]
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        for field in [self.path.to_string(), self.verdict.as_str().to_string()] {
            h.update((field.len() as u64).to_be_bytes());
            h.update(field.as_bytes());
        }
        // An absent code and an empty one are DIFFERENT records, and collapsing
        // them with `unwrap_or_default()` created a deterministic collision in
        // evidence whose whole purpose is telling records apart. The presence
        // tag is hashed before the content so the two can never coincide.
        match &self.audit_code {
            None => h.update([0u8]),
            Some(code) => {
                h.update([1u8]);
                h.update((code.len() as u64).to_be_bytes());
                h.update(code.as_bytes());
            }
        }
        format!("sha-256:{}", hex_lower(&h.finalize()))
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What shadow mode decided, and what it observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowOutcome {
    /// The verdict that actually governs this step. Always the legacy one in
    /// shadow mode — that is what "shadow" means.
    pub decided: Verdict,
    /// The mismatch to record, when the paths disagreed.
    pub mismatch: Option<Mismatch>,
}

/// Which way a disagreement points, operationally.
///
/// Three-valued rather than a bool: two paths can disagree while BOTH deny
/// execution (`Violation` vs `InsufficientEvidence`), and reporting that as
/// "not more restrictive" made it indistinguishable from a genuine
/// authorization gap — corrupting the very statistics the promotion decision
/// rests on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MismatchDirection {
    /// Legacy permitted, AAT denied: a potential outage on promotion.
    AatMoreRestrictive,
    /// Legacy denied, AAT permitted: a potential authorization gap.
    AatMorePermissive,
    /// The verdicts differ but neither permits execution. No execution
    /// difference, but the recorded reason would change.
    BothDeny,
}

/// A recorded disagreement between the two paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mismatch {
    /// Always [`MISMATCH_VERB`].
    ///
    /// An owned `String`, not `&'static str`: the derived deserializer would
    /// otherwise demand a `'static` input buffer, so `Mismatch` would not
    /// implement `DeserializeOwned` and `serde_json::from_reader` could not
    /// reconstruct evidence this crate advertises as durable.
    pub verb: String,
    /// Digest of the legacy verdict.
    pub legacy_digest: String,
    /// Digest of the AAT verdict.
    pub aat_digest: String,
    /// The legacy verdict, for a reader who should not have to invert a hash.
    pub legacy_verdict: Verdict,
    /// The AAT verdict.
    pub aat_verdict: Verdict,
    /// Which way the disagreement points.
    pub direction: MismatchDirection,
}

/// The gate's accumulated evidence, independent of any process.
///
/// Separated from the comparator so it can be persisted and restored. A gate
/// held only in memory silently resets on restart: after another clean run the
/// comparator would report ready while the durable chain still contained an
/// earlier mismatch, which is the opposite of the "one mismatch blocks forever"
/// guarantee.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateState {
    /// Governed steps observed.
    pub steps: u64,
    /// Disagreements recorded.
    pub mismatches: u64,
    /// Unix time of the first observed step.
    pub first_step_unix: Option<u64>,
    /// Unix time of the most recent observed step.
    pub last_step_unix: Option<u64>,
}

/// Runs both paths and decides with legacy.
#[derive(Clone, Debug, Default)]
pub struct ShadowComparator {
    state: GateState,
}

impl ShadowComparator {
    /// A comparator that has observed nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resume from previously persisted evidence.
    ///
    /// The caller is responsible for loading this from the durable chain; the
    /// point is that resuming is *possible*, so a restart cannot quietly
    /// discharge a recorded mismatch.
    #[must_use]
    pub fn resume(state: GateState) -> Self {
        Self { state }
    }

    /// The evidence accumulated so far, for persisting across restarts.
    #[must_use]
    pub fn state(&self) -> GateState {
        self.state
    }

    /// Compare one governed step.
    ///
    /// Returns the legacy verdict as the decision **always** — including when
    /// the paths disagree and including when AAT is stricter. A comparator that
    /// sometimes lets the shadow path decide is not running in shadow mode.
    ///
    /// The two arguments have distinct types, so a caller cannot swap them and
    /// silently invert which path governs.
    pub fn compare(
        &mut self,
        legacy: &LegacyVerdict,
        aat: &AatVerdict,
        now_unix: u64,
    ) -> ShadowOutcome {
        let legacy = legacy.inner();
        let aat = aat.inner();

        self.state.steps += 1;
        self.state.first_step_unix.get_or_insert(now_unix);
        self.state.last_step_unix = Some(now_unix);

        // Compare the VERDICT only. Two paths may reach the same answer by
        // different internal reasoning, and treating a differing audit code as
        // a mismatch would make the promotion gate unreachable for a reason
        // that does not affect any decision.
        if legacy.verdict == aat.verdict {
            return ShadowOutcome {
                decided: legacy.verdict,
                mismatch: None,
            };
        }

        self.state.mismatches += 1;
        let direction = match (
            legacy.verdict.permits_execution(),
            aat.verdict.permits_execution(),
        ) {
            (true, false) => MismatchDirection::AatMoreRestrictive,
            (false, true) => MismatchDirection::AatMorePermissive,
            // Both deny by different reasoning: no execution difference, but
            // the recorded reason would change on promotion.
            (false, false) => MismatchDirection::BothDeny,
            // Unreachable: equal permission with differing verdicts would mean
            // two distinct verdicts both permit, and only Compliant does.
            (true, true) => MismatchDirection::BothDeny,
        };

        ShadowOutcome {
            decided: legacy.verdict,
            mismatch: Some(Mismatch {
                verb: MISMATCH_VERB.to_string(),
                legacy_digest: legacy.digest(),
                aat_digest: aat.digest(),
                legacy_verdict: legacy.verdict,
                aat_verdict: aat.verdict,
                direction,
            }),
        }
    }

    /// Governed steps compared so far.
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.state.steps
    }

    /// Disagreements recorded so far.
    #[must_use]
    pub fn mismatches(&self) -> u64 {
        self.state.mismatches
    }

    /// Seconds spanned by the observed steps.
    #[must_use]
    pub fn elapsed_secs(&self) -> u64 {
        match (self.state.first_step_unix, self.state.last_step_unix) {
            (Some(a), Some(b)) => b.saturating_sub(a),
            _ => 0,
        }
    }

    /// Whether the AAT path has earned the right to decide.
    ///
    /// Requires **zero** mismatches, at least [`PROMOTION_MIN_STEPS`] governed
    /// steps, **and** at least [`PROMOTION_MIN_SECONDS`] of wall-clock span —
    /// "whichever is later", so both bounds must be satisfied, not either.
    /// Volume alone can be accumulated in an afternoon on one workload; the
    /// time bound is what exposes the comparison to varied conditions.
    #[must_use]
    pub fn promotion_ready(&self) -> bool {
        self.state.mismatches == 0
            && self.state.steps >= PROMOTION_MIN_STEPS
            && self.elapsed_secs() >= PROMOTION_MIN_SECONDS
    }

    /// Why promotion is not yet available, for an operator asking.
    #[must_use]
    pub fn promotion_blockers(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.state.mismatches > 0 {
            out.push(format!(
                "{} mismatch(es) recorded; the gate requires zero",
                self.state.mismatches
            ));
        }
        if self.state.steps < PROMOTION_MIN_STEPS {
            out.push(format!(
                "{} of {PROMOTION_MIN_STEPS} governed steps observed",
                self.state.steps
            ));
        }
        if self.elapsed_secs() < PROMOTION_MIN_SECONDS {
            out.push(format!(
                "{} of {PROMOTION_MIN_SECONDS} seconds elapsed",
                self.elapsed_secs()
            ));
        }
        out
    }
}
