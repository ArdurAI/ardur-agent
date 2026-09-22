//! #543: durable per-event evidence for the native-to-ER mirror.
//!
//! Phase 1 (#560) mirrors one ER per **committed round** from the digests the
//! native receipt already carries. That cannot reconstruct what the verifier
//! contract actually wants — **one ER per evaluated event** — because the
//! native receipt deliberately does not carry tool arguments/outputs inline,
//! and refusal / timeout / scan failure exit before the round's append while
//! memory work happens after it.
//!
//! This module is the durable fact form the mirror projects from:
//!
//! - [`PreEffectRecord`] — the immutable authorization inputs of one
//!   evaluated event, persisted **before** the effect: stable event identity,
//!   the verified grant facts, the normalized invocation classification, and
//!   the canonical arguments (inline up to
//!   [`MAX_INLINE_ARGUMENTS_BYTES`]; over that cap the record keeps only the
//!   content-addressing digests, and projection degrades to explicit
//!   `insufficient_evidence` rather than guessed provenance).
//! - [`PostEffectRecord`] — the terminal observation: an observed effect
//!   (output digest + incurred cost + output-admission decision), a typed
//!   denial, or an explicitly **unknown** outcome (timeout, execution error).
//!
//! Records serialize one JSON object per line into the emitter-owned evidence
//! journal (`governance/events.jsonl` beside the ER chain). A crash replay
//! reads the journal and re-projects every terminal event the ER chain does
//! not yet carry — idempotently, because the event id (and therefore the ER
//! `step_id`, run nonce, and receipt id) is a deterministic hash of recorded
//! material, never of process state. Replay **never** re-executes a tool:
//! this module has no access to a tool registry, only to recorded facts.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ardur_core_types::CostTuple;

use crate::er::{ActionClass, PublicDenialReason, SideEffectClass};
use crate::error::GovernanceError;
use crate::hash::{sha256, sha256_hex};
use crate::project::{GrantFacts, ToolInvocation, invocation_digests};

/// The evidence record format version written by this build.
pub const EVIDENCE_RECORD_VERSION: u32 = 1;

/// Canonical arguments larger than this are not inlined into the durable
/// record; the record keeps the content-addressing digests only, and the
/// event's ER reports `insufficient_evidence`
/// (`arguments_evidence_omitted`) rather than claiming compliance over inputs
/// the evidence cannot show. 1 MiB is far beyond any tool-call envelope the
/// runtime admits.
pub const MAX_INLINE_ARGUMENTS_BYTES: usize = 1 << 20;

/// The capability name a memory-write event is recorded under (matches
/// `ardur_memory::MEMORY_WRITE_CAPABILITY`; re-declared here so the
/// governance crate takes no dependency on the memory backend crate).
pub const MEMORY_WRITE_TOOL: &str = "memory.write";

/// Which evaluated-event family a record belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A tool invocation evaluated at the runtime's tool-call boundary.
    ToolInvocation,
    /// A turn-record write into the configured memory backend, evaluated
    /// under a dedicated `memory.write` re-verification after the round's
    /// commit.
    MemoryWrite,
}

/// The admission decision on a completed tool's output before it re-enters
/// the transcript. `NotScanned` is deliberately absent: the record is written
/// at the scan decision, so a persisted completion is always decisively
/// admitted, blocked, or — when the scanner itself failed operationally —
/// undetermined.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceOutputAdmission {
    /// The output scan admitted the result.
    Allowed,
    /// The output scan blocked the result (the effect still happened).
    Blocked,
    /// The scanner failed operationally (a filter error, not a block
    /// verdict): admission could not be determined, and the projection must
    /// report `insufficient_evidence`, never a guessed violation.
    Undetermined,
}

/// An observed, completed effect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletedOutcome {
    /// Hex SHA-256 of the serialized output the tool returned.
    pub output_digest: String,
    /// The cost the invocation billed.
    pub cost: CostTuple,
    /// The output-admission decision.
    pub output_admission: EvidenceOutputAdmission,
}

/// A typed denial: the event was evaluated and rejected before any effect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeniedOutcome {
    /// The user-facing denial reason (schema vocabulary).
    pub public: PublicDenialReason,
    /// The stable audit-only denial code (e.g. `tool_not_allowed`).
    pub internal: String,
}

/// The terminal observation of an evaluated event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EventOutcome {
    /// The effect completed and was observed (digest + cost), with the
    /// output-admission decision.
    Completed(CompletedOutcome),
    /// The event was denied at an admission gate; no effect occurred.
    Denied(DeniedOutcome),
    /// The invocation returned an execution error. A tool error does not
    /// imply no external effect occurred, so the outcome is honestly unknown.
    FailedUnknown,
    /// The invocation exceeded its deadline after a possible dispatch; no
    /// result was observed. The effect is unknown.
    TimeoutUnknown,
}

/// The identity scope of a tool-invocation event: which session and which
/// round of which turn the call was evaluated in, its batch ordinal, and the
/// provider-assigned call id. The round's request id (minted fresh per
/// provider round) is the discriminator that keeps the event id unique even
/// when a provider reuses a call id at the same ordinal on a later turn of
/// the same session — the case #543 review caught a bare
/// (session, iteration, ordinal, call id) seed collapsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventScope<'a> {
    /// The session the event belongs to (ER `trace_id`).
    pub session_id: &'a str,
    /// The round's request id (`CompletionRequest::request_id`, a fresh
    /// UUIDv4 per round).
    pub request_id: &'a str,
    /// The tool-loop iteration (1-based).
    pub iteration: u32,
    /// The call's ordinal within the round's requested batch.
    pub tool_ordinal: u32,
    /// The provider-assigned call id.
    pub call_id: &'a str,
}

/// The immutable authorization inputs of one evaluated event, durable before
/// the effect. Everything an ER projection needs is either carried here or
/// derivable from what is carried — a replay never consults live state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreEffectRecord {
    /// Record format version ([`EVIDENCE_RECORD_VERSION`]).
    pub v: u32,
    /// Stable event identity: `ev:<40 hex>` over
    /// (session, round request id, iteration, tool ordinal, call id) — or
    /// the round receipt id for a memory write. Deterministic, so a crash
    /// replay reconstructs the same identity instead of minting a sibling.
    pub event_id: String,
    /// The event family.
    pub kind: EventKind,
    /// The session the event belongs to (ER `trace_id`).
    pub session_id: String,
    /// The tool-loop iteration (1-based) the event was evaluated in; 0 for a
    /// memory write (which is keyed off the round receipt instead).
    pub iteration: u32,
    /// The call's ordinal within the round's requested batch.
    pub tool_ordinal: u32,
    /// The provider-assigned call id (`memory.write:<receipt>` for a memory
    /// write, which has no provider call).
    pub call_id: String,
    /// The tool / capability invoked.
    pub tool: String,
    /// When the inputs were recorded (Unix milliseconds).
    pub recorded_at_ms: u64,
    /// The verified grant id (cap-token `token_id`; ER `grant_id`).
    pub grant_id: String,
    /// The verified subject (ER `actor`).
    pub actor: String,
    /// The verified remaining budget at evaluation time.
    pub budget_remaining: u64,
    /// The normalized action family.
    pub action_class: ActionClass,
    /// The normalized target (the tool name; per-tool target extraction would
    /// be guessing at argument semantics).
    pub target: String,
    /// The coarse resource category, classified from the tool's declared
    /// capability surface.
    pub resource_family: String,
    /// The side-effect family, classified from the tool's declared
    /// capability surface (the *authorized intent*, not a post-hoc claim).
    pub side_effect_class: SideEffectClass,
    /// The canonical invocation arguments — the authorization inputs
    /// themselves. `None` when they exceeded [`MAX_INLINE_ARGUMENTS_BYTES`]:
    /// the digests below still bind the event, and projection reports
    /// `insufficient_evidence` for an otherwise-compliant outcome.
    pub arguments: Option<Value>,
    /// Hex SHA-256 of the JCS-canonical arguments — always recorded, so a
    /// digest-only record still content-addresses the inputs.
    pub arguments_hash: String,
    /// Base64url SHA-256 of the JCS-canonical invocation envelope (the ER
    /// `digestObject.value`) — always recorded.
    pub invocation_digest: String,
}

impl PreEffectRecord {
    /// Record the authorization inputs of a tool invocation, computing the
    /// content-addressing digests through the same
    /// [`invocation_digests`](crate::project::invocation_digests) the live ER
    /// projector uses.
    pub fn tool_invocation(
        scope: &EventScope<'_>,
        tool: &str,
        grant: &GrantFacts,
        classification: &InvocationClassification,
        arguments: Value,
        recorded_at_ms: u64,
    ) -> Self {
        let event_id = tool_event_id(
            scope.session_id,
            scope.request_id,
            scope.iteration,
            scope.tool_ordinal,
            scope.call_id,
        );
        let (arguments_hash, invocation_digest) = {
            let call = ToolInvocation {
                tool,
                action_class: classification.action_class,
                target: &classification.target,
                resource_family: &classification.resource_family,
                side_effect_class: classification.side_effect_class,
                arguments: &arguments,
            };
            invocation_digests(&grant.grant_id, &call)
        };
        let inline = serde_json::to_vec(&arguments)
            .map(|bytes| bytes.len() <= MAX_INLINE_ARGUMENTS_BYTES)
            .unwrap_or(false);
        Self {
            v: EVIDENCE_RECORD_VERSION,
            event_id,
            kind: EventKind::ToolInvocation,
            session_id: scope.session_id.to_string(),
            iteration: scope.iteration,
            tool_ordinal: scope.tool_ordinal,
            call_id: scope.call_id.to_string(),
            tool: tool.to_string(),
            recorded_at_ms,
            grant_id: grant.grant_id.clone(),
            actor: grant.actor.clone(),
            budget_remaining: grant.budget_remaining,
            action_class: classification.action_class,
            target: classification.target.clone(),
            resource_family: classification.resource_family.clone(),
            side_effect_class: classification.side_effect_class,
            arguments: inline.then_some(arguments),
            arguments_hash,
            invocation_digest: invocation_digest.value,
        }
    }

    /// Record the authorization inputs of a turn's memory write. The record
    /// content is not inlined — it can be arbitrarily large and is not an
    /// *authorization* input; the descriptor (the content digest plus the
    /// owning round's receipt id) is the content-addressed evidence.
    pub fn memory_write(
        session_id: &str,
        round_receipt_id: &str,
        grant: &GrantFacts,
        record_digest: &str,
        recorded_at_ms: u64,
    ) -> Self {
        let event_id = memory_event_id(session_id, round_receipt_id);
        let arguments = serde_json::json!({
            "record_digest": record_digest,
            "round_receipt_id": round_receipt_id,
            "source": "turn_record",
        });
        let classification = InvocationClassification {
            action_class: ActionClass::Write,
            target: MEMORY_WRITE_TOOL.to_string(),
            resource_family: "memory".to_string(),
            side_effect_class: SideEffectClass::InternalWrite,
        };
        let (arguments_hash, invocation_digest) = {
            let call = ToolInvocation {
                tool: &classification.target,
                action_class: classification.action_class,
                target: &classification.target,
                resource_family: &classification.resource_family,
                side_effect_class: classification.side_effect_class,
                arguments: &arguments,
            };
            invocation_digests(&grant.grant_id, &call)
        };
        Self {
            v: EVIDENCE_RECORD_VERSION,
            event_id,
            kind: EventKind::MemoryWrite,
            session_id: session_id.to_string(),
            iteration: 0,
            tool_ordinal: 0,
            call_id: format!("memory.write:{round_receipt_id}"),
            tool: classification.target.clone(),
            recorded_at_ms,
            grant_id: grant.grant_id.clone(),
            actor: grant.actor.clone(),
            budget_remaining: grant.budget_remaining,
            action_class: classification.action_class,
            target: classification.target,
            resource_family: classification.resource_family,
            side_effect_class: classification.side_effect_class,
            arguments: Some(arguments),
            arguments_hash,
            invocation_digest: invocation_digest.value,
        }
    }

    /// Rebuild the normalized [`ToolInvocation`] the record carries, for
    /// digest recomputation at projection time.
    pub fn as_tool_invocation<'a>(&'a self, arguments: &'a Value) -> ToolInvocation<'a> {
        ToolInvocation {
            tool: &self.tool,
            action_class: self.action_class,
            target: &self.target,
            resource_family: &self.resource_family,
            side_effect_class: self.side_effect_class,
            arguments,
        }
    }
}

/// The terminal observation of one evaluated event, durable at the event's
/// terminal point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostEffectRecord {
    /// Record format version ([`EVIDENCE_RECORD_VERSION`]).
    pub v: u32,
    /// The event this observation terminates (must equal the pre-effect
    /// record's `event_id`).
    pub event_id: String,
    /// When the observation was recorded (Unix milliseconds).
    pub recorded_at_ms: u64,
    /// What was observed — including, explicitly, an *unknown* outcome.
    pub outcome: EventOutcome,
}

impl PostEffectRecord {
    /// Build the terminal record for an event.
    pub fn new(pre: &PreEffectRecord, recorded_at_ms: u64, outcome: EventOutcome) -> Self {
        Self {
            v: EVIDENCE_RECORD_VERSION,
            event_id: pre.event_id.clone(),
            recorded_at_ms,
            outcome,
        }
    }
}

/// One journal line: a pre- or post-effect record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EvidenceRecord {
    /// Immutable authorization inputs (before the effect).
    PreEffect(Box<PreEffectRecord>),
    /// Terminal observation (at the event's end).
    PostEffect(PostEffectRecord),
}

impl EvidenceRecord {
    /// Serialize to one journal line (no trailing newline).
    pub fn to_line(&self) -> Result<String, GovernanceError> {
        serde_json::to_string(self)
            .map_err(|e| GovernanceError::Io(format!("evidence record serialize: {e}")))
    }

    /// Parse one journal line.
    ///
    /// # Errors
    ///
    /// [`GovernanceError::Io`] on malformed JSON, unknown fields, or a record
    /// whose `v` is not [`EVIDENCE_RECORD_VERSION`]: a record written by a
    /// different format version must fail closed at the open-time sweep, not
    /// be interpreted with this build's semantics.
    pub fn from_line(line: &str) -> Result<Self, GovernanceError> {
        let record: Self = serde_json::from_str(line)
            .map_err(|e| GovernanceError::Io(format!("evidence record parse: {e}")))?;
        let v = match &record {
            EvidenceRecord::PreEffect(pre) => pre.v,
            EvidenceRecord::PostEffect(post) => post.v,
        };
        if v != EVIDENCE_RECORD_VERSION {
            return Err(GovernanceError::Io(format!(
                "evidence record version {v} is not supported by this build \
                 (v{EVIDENCE_RECORD_VERSION}); refusing to replay"
            )));
        }
        Ok(record)
    }
}

/// The normalized invocation classification for an event, decided by the
/// runtime from the tool's **declared** capability surface (never from the
/// argument values — classifying from arguments would be guessing at their
/// semantics).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvocationClassification {
    /// The normalized action family.
    pub action_class: ActionClass,
    /// The normalized target.
    pub target: String,
    /// The coarse resource category.
    pub resource_family: String,
    /// The side-effect family.
    pub side_effect_class: SideEffectClass,
}

/// The stable identity of a tool-invocation event: a deterministic hash over
/// the unambiguous (JSON-encoded) tuple of session, round request id,
/// iteration, ordinal and provider call id, so a replayed projection names
/// the same event. The round request id keeps the identity unique across
/// turns of a session even when a provider reuses call ids.
pub fn tool_event_id(
    session_id: &str,
    request_id: &str,
    iteration: u32,
    tool_ordinal: u32,
    call_id: &str,
) -> String {
    let seed = serde_json::to_vec(&serde_json::json!([
        session_id,
        request_id,
        iteration,
        tool_ordinal,
        call_id
    ]))
    .unwrap_or_default();
    format!("ev:{}", &sha256_hex(&seed)[..40])
}

/// The stable identity of a memory-write event: the write happens at most
/// once per committed round, so the round's receipt id keys it.
pub fn memory_event_id(session_id: &str, round_receipt_id: &str) -> String {
    let seed = serde_json::to_vec(&serde_json::json!([
        session_id,
        "memory.write",
        round_receipt_id
    ]))
    .unwrap_or_default();
    format!("ev:{}", &sha256_hex(&seed)[..40])
}

/// The deterministic per-event run nonce (base64url, 32 chars — within the
/// schema's 16..=128 bound). Deriving it from the event id makes the whole ER
/// claim set (modulo the chain link) replay-stable across processes: a live
/// projection and a post-crash sweep mint the *same* receipt id for the same
/// records, which is what makes the sweep idempotent.
pub fn event_run_nonce(event_id: &str) -> String {
    let digest = sha256(format!("ardur-er-event-nonce/v1|{event_id}").as_bytes());
    B64URL.encode(&digest[..24])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::er::SideEffectClass;

    fn grant() -> GrantFacts {
        GrantFacts {
            grant_id: "7c9e6679-7425-40de-944b-e07fc1f90ae7".to_string(),
            actor: "spiffe://ardur/user/test".to_string(),
            budget_remaining: 1_000,
        }
    }

    fn classification() -> InvocationClassification {
        InvocationClassification {
            action_class: ActionClass::Observe,
            target: "echo".to_string(),
            resource_family: "tool".to_string(),
            side_effect_class: SideEffectClass::None,
        }
    }

    #[test]
    fn event_identity_is_deterministic_and_field_sensitive() {
        let a = tool_event_id("session-1", "request-1", 1, 0, "call-1");
        let b = tool_event_id("session-1", "request-1", 1, 0, "call-1");
        assert_eq!(a, b, "same facts, same identity");
        assert!(a.starts_with("ev:") && a.len() == 43);
        assert_ne!(
            a,
            tool_event_id("session-1", "request-1", 1, 1, "call-1"),
            "ordinal matters"
        );
        assert_ne!(
            a,
            tool_event_id("session-1", "request-1", 2, 0, "call-1"),
            "iteration matters"
        );
        assert_ne!(
            a,
            tool_event_id("session-2", "request-1", 1, 0, "call-1"),
            "session matters"
        );
        assert_ne!(
            a,
            tool_event_id("session-1", "request-2", 1, 0, "call-1"),
            "round request id matters: a provider reusing a call id on a later              turn of the same session cannot collide"
        );
        assert_ne!(
            a,
            tool_event_id("session-1", "request-1", 1, 0, "call-2"),
            "call id matters"
        );
        // No delimiter ambiguity: shifting text across fields changes the id.
        assert_ne!(
            tool_event_id("ab", "r", 1, 0, "c"),
            tool_event_id("a", "r", 1, 0, "bc"),
            "JSON-tuple seeding cannot shift bytes across fields"
        );
    }

    #[test]
    fn run_nonce_is_deterministic_and_schema_shaped() {
        let n = event_run_nonce("ev:abc");
        assert_eq!(n, event_run_nonce("ev:abc"));
        assert_ne!(n, event_run_nonce("ev:abd"));
        assert!((16..=128).contains(&n.len()));
        assert!(
            n.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        );
    }

    #[test]
    fn pre_record_inlines_small_arguments_and_computes_matching_digests() {
        let pre = PreEffectRecord::tool_invocation(
            &EventScope {
                session_id: "session-1",
                request_id: "request-1",
                iteration: 1,
                tool_ordinal: 0,
                call_id: "call-1",
            },
            "echo",
            &grant(),
            &classification(),
            serde_json::json!({ "text": "hi" }),
            1_750_000_000_000,
        );
        let args = pre.arguments.clone().expect("small arguments inline");
        let call = pre.as_tool_invocation(&args);
        let (hash, digest) = invocation_digests(&pre.grant_id, &call);
        assert_eq!(hash, pre.arguments_hash);
        assert_eq!(digest.value, pre.invocation_digest);
    }

    #[test]
    fn pre_record_omits_oversized_arguments_but_keeps_digests() {
        let big = "x".repeat(MAX_INLINE_ARGUMENTS_BYTES + 1);
        let pre = PreEffectRecord::tool_invocation(
            &EventScope {
                session_id: "session-1",
                request_id: "request-1",
                iteration: 1,
                tool_ordinal: 0,
                call_id: "call-1",
            },
            "echo",
            &grant(),
            &classification(),
            serde_json::json!({ "blob": big }),
            1_750_000_000_000,
        );
        assert!(
            pre.arguments.is_none(),
            "over-cap arguments are not inlined"
        );
        assert_eq!(
            pre.arguments_hash.len(),
            64,
            "the digest still content-addresses them"
        );
        assert!(!pre.invocation_digest.is_empty());
    }

    #[test]
    fn journal_lines_round_trip_and_reject_unknown_fields() {
        let pre = PreEffectRecord::tool_invocation(
            &EventScope {
                session_id: "session-1",
                request_id: "request-1",
                iteration: 1,
                tool_ordinal: 0,
                call_id: "call-1",
            },
            "echo",
            &grant(),
            &classification(),
            serde_json::json!({}),
            1_750_000_000_000,
        );
        let post = PostEffectRecord::new(&pre, 1_750_000_000_100, EventOutcome::TimeoutUnknown);
        for record in [
            EvidenceRecord::PreEffect(Box::new(pre)),
            EvidenceRecord::PostEffect(post),
        ] {
            let line = record.to_line().expect("serialize");
            let back = EvidenceRecord::from_line(&line).expect("parse");
            assert_eq!(record, back);
        }
        // A record carrying a field this build does not know fails closed.
        let tampered = "{\"pre_effect\":{\"v\":1,\"event_id\":\"ev:x\",\"kind\":\"tool_invocation\",\"session_id\":\"s\",\"iteration\":1,\"tool_ordinal\":0,\"call_id\":\"c\",\"tool\":\"t\",\"recorded_at_ms\":1,\"grant_id\":\"g\",\"actor\":\"a\",\"budget_remaining\":0,\"action_class\":\"observe\",\"target\":\"t\",\"resource_family\":\"tool\",\"side_effect_class\":\"none\",\"arguments\":null,\"arguments_hash\":\"h\",\"invocation_digest\":\"d\",\"smuggled\":true}}";
        assert!(EvidenceRecord::from_line(tampered).is_err());
    }

    #[test]
    fn memory_write_record_uses_the_descriptor_as_content_addressed_evidence() {
        let pre = PreEffectRecord::memory_write(
            "session-1",
            "9b1d2c3b-0000-4000-8000-000000000000",
            &grant(),
            &sha256_hex(b"record"),
            1_750_000_000_000,
        );
        assert_eq!(pre.kind, EventKind::MemoryWrite);
        assert_eq!(pre.side_effect_class, SideEffectClass::InternalWrite);
        let args = pre.arguments.clone().expect("descriptor inline");
        assert_eq!(
            args["record_digest"],
            serde_json::json!(sha256_hex(b"record"))
        );
        let call = pre.as_tool_invocation(&args);
        let (hash, digest) = invocation_digests(&pre.grant_id, &call);
        assert_eq!(hash, pre.arguments_hash);
        assert_eq!(digest.value, pre.invocation_digest);
    }
}
