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
        for field in [
            self.path.to_string(),
            self.verdict.as_str().to_string(),
            self.audit_code.clone().unwrap_or_default(),
        ] {
            h.update((field.len() as u64).to_be_bytes());
            h.update(field.as_bytes());
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

/// A recorded disagreement between the two paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mismatch {
    /// Always [`MISMATCH_VERB`], carried so a serialized record is
    /// self-describing to a consumer that has never seen this crate.
    pub verb: &'static str,
    /// Digest of the legacy verdict.
    pub legacy_digest: String,
    /// Digest of the AAT verdict.
    pub aat_digest: String,
    /// The legacy verdict, for a reader who should not have to invert a hash.
    pub legacy_verdict: Verdict,
    /// The AAT verdict.
    pub aat_verdict: Verdict,
    /// Whether the AAT path would have been *more* restrictive.
    ///
    /// The direction is the operationally important part: an AAT path that
    /// denies what legacy allowed is a potential outage on promotion, while the
    /// reverse is a potential authorization gap. They are not symmetric and a
    /// bare "mismatch" count hides which one is happening.
    pub aat_more_restrictive: bool,
}

/// Runs both paths and decides with legacy.
#[derive(Clone, Debug, Default)]
pub struct ShadowComparator {
    steps: u64,
    mismatches: u64,
    first_step_unix: Option<u64>,
    last_step_unix: Option<u64>,
}

impl ShadowComparator {
    /// A comparator that has observed nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Compare one governed step.
    ///
    /// Returns the legacy verdict as the decision **always** — including when
    /// the paths disagree and including when AAT is stricter. A comparator that
    /// sometimes lets the shadow path decide is not running in shadow mode.
    ///
    /// # Panics
    /// Never. Argument order is enforced by [`Path`], so a caller cannot
    /// silently swap the two paths and invert the recorded direction.
    pub fn compare(
        &mut self,
        legacy: &PathVerdict,
        aat: &PathVerdict,
        now_unix: u64,
    ) -> ShadowOutcome {
        debug_assert_eq!(legacy.path, Path::Legacy, "legacy slot must carry Legacy");
        debug_assert_eq!(aat.path, Path::Aat, "aat slot must carry Aat");

        self.steps += 1;
        self.first_step_unix.get_or_insert(now_unix);
        self.last_step_unix = Some(now_unix);

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

        self.mismatches += 1;
        ShadowOutcome {
            decided: legacy.verdict,
            mismatch: Some(Mismatch {
                verb: MISMATCH_VERB,
                legacy_digest: legacy.digest(),
                aat_digest: aat.digest(),
                legacy_verdict: legacy.verdict,
                aat_verdict: aat.verdict,
                aat_more_restrictive: legacy.verdict.permits_execution()
                    && !aat.verdict.permits_execution(),
            }),
        }
    }

    /// Governed steps compared so far.
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Disagreements recorded so far.
    #[must_use]
    pub fn mismatches(&self) -> u64 {
        self.mismatches
    }

    /// Seconds spanned by the observed steps.
    #[must_use]
    pub fn elapsed_secs(&self) -> u64 {
        match (self.first_step_unix, self.last_step_unix) {
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
        self.mismatches == 0
            && self.steps >= PROMOTION_MIN_STEPS
            && self.elapsed_secs() >= PROMOTION_MIN_SECONDS
    }

    /// Why promotion is not yet available, for an operator asking.
    #[must_use]
    pub fn promotion_blockers(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.mismatches > 0 {
            out.push(format!(
                "{} mismatch(es) recorded; the gate requires zero",
                self.mismatches
            ));
        }
        if self.steps < PROMOTION_MIN_STEPS {
            out.push(format!(
                "{} of {PROMOTION_MIN_STEPS} governed steps observed",
                self.steps
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
