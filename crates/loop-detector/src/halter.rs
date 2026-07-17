//! The halt / kill / override side-effect surface.
//!
//! The halter turns a detector verdict into the receipt-bearing report the
//! runtime acts on. Like the detector it computes no signatures and does no
//! signing — it names the verb and anchors the evidence digest; the runtime
//! mints the receipt and, for a kill, tears the run down. The one piece of
//! mutable state it touches is the override flow, which returns a `Halted` run
//! to `Healthy`.

use ardur_receipt::{Sha256Digest, VerbObject};

use crate::detector::LoopDetectorState;
use crate::error::HalterError;
use crate::ids::RunId;
use crate::receipt::{evidence_payload_digest, verb, verbs};
use crate::sealed::Sealed;
use crate::types::{HaltStatus, KillReason, LoopEvidence, Signal};

/// The receipt a halt produces: the `agent.loop.halted.v1` verb and the digest
/// anchoring its evidence on chain.
#[derive(Clone, Debug, PartialEq)]
pub struct HaltReport {
    /// `agent.loop.halted.v1`.
    pub verb: VerbObject,
    /// SHA-256 of the halt evidence.
    pub evidence_digest: Sha256Digest,
}

/// The receipt a kill produces: the `agent.runaway.killed.v1` verb, the reason,
/// and the evidence digest.
#[derive(Clone, Debug, PartialEq)]
pub struct KillReport {
    /// `agent.runaway.killed.v1`.
    pub verb: VerbObject,
    /// Why the run was killed.
    pub reason: KillReason,
    /// SHA-256 of the kill evidence.
    pub evidence_digest: Sha256Digest,
}

/// An operator's request to override a halt and resume the run.
#[derive(Clone, Debug, PartialEq)]
pub struct OverrideRequest {
    /// The run to resume.
    pub run_id: RunId,
    /// The operator's justification — must be non-empty; the runtime
    /// sentinel-scans it before it lands in the override receipt.
    pub reason: String,
}

/// The result of an accepted override: the `agent.loop.detection_overridden.v1`
/// verb and the signal the override cleared.
#[derive(Clone, Debug, PartialEq)]
pub struct OverrideReport {
    /// `agent.loop.detection_overridden.v1`.
    pub verb: VerbObject,
    /// The run that resumed.
    pub run_id: RunId,
    /// The signal the halt had been holding.
    pub previous_signal: Signal,
}

/// The halter facade. Sealed: [`InMemoryRunawayHalter`] is the single workspace
/// implementation.
pub trait RunawayHalter: Sealed {
    /// Build the halt report for a grace-expired trip.
    fn halt(&self, evidence: &LoopEvidence) -> Result<HaltReport, HalterError>;

    /// Build the kill report for an escalated trip.
    fn kill(&self, reason: KillReason, evidence: &LoopEvidence) -> Result<KillReport, HalterError>;

    /// Accept an operator override of a halted run, returning it to `Healthy`.
    /// Errors if the reason is empty or the run is not halted.
    fn override_halt(
        &self,
        request: &OverrideRequest,
        state: &mut LoopDetectorState,
    ) -> Result<OverrideReport, HalterError>;
}

/// The Phase-1 in-memory halter.
#[derive(Clone, Copy, Debug, Default)]
pub struct InMemoryRunawayHalter;

impl InMemoryRunawayHalter {
    /// Construct the default halter.
    pub fn new() -> Self {
        Self
    }
}

impl Sealed for InMemoryRunawayHalter {}

impl RunawayHalter for InMemoryRunawayHalter {
    fn halt(&self, evidence: &LoopEvidence) -> Result<HaltReport, HalterError> {
        Ok(HaltReport {
            verb: verb(verbs::LOOP_HALTED).expect("LOOP_HALTED is a valid verb"),
            evidence_digest: evidence_payload_digest(evidence),
        })
    }

    fn kill(&self, reason: KillReason, evidence: &LoopEvidence) -> Result<KillReport, HalterError> {
        Ok(KillReport {
            verb: verb(verbs::RUNAWAY_KILLED).expect("RUNAWAY_KILLED is a valid verb"),
            reason,
            evidence_digest: evidence_payload_digest(evidence),
        })
    }

    fn override_halt(
        &self,
        request: &OverrideRequest,
        state: &mut LoopDetectorState,
    ) -> Result<OverrideReport, HalterError> {
        if request.reason.trim().is_empty() {
            return Err(HalterError::EmptyOverrideReason);
        }
        let previous_signal = match &state.halt_status {
            HaltStatus::Halted { signal } => signal.clone(),
            _ => return Err(HalterError::NotHalted),
        };
        // The override resets the active trips and returns the run to healthy;
        // it does NOT lower thresholds, so a persisting loop trips again.
        state.halt_status = HaltStatus::Healthy;
        state.clear_active_trips();
        Ok(OverrideReport {
            verb: verb(verbs::DETECTION_OVERRIDDEN).expect("DETECTION_OVERRIDDEN is a valid verb"),
            run_id: request.run_id,
            previous_signal,
        })
    }
}
