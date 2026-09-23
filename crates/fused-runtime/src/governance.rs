//! The opt-in ER mirror emitter (#502 Seam B7) with #543 durable per-event
//! evidence.
//!
//! [`ErMirrorEmitter`] is the runtime's default [`GovernanceEmitter`]. It
//! projects each **committed** round's verified admission facts into an MCEP
//! Execution Receipt, and — since #543 — each **evaluated event** (tool
//! invocation, memory write) into its own ER, from durable pre/post-effect
//! records. Both ER kinds sign with the same P-256 custody as the native
//! receipt chain (one key, one JWKS) and append to a hash-chained mirror log —
//! `governance/er-chain.jsonl` under the operator's data dir — using the same
//! hardened no-follow, fsync-ing append the native receipt log uses. The
//! evidence records themselves live in a sibling journal,
//! `governance/events.jsonl`, written with the same discipline.
//!
//! #543 boundaries, deliberately:
//!
//! - **One ER per evaluated event.** Each tool call that reaches the
//!   admission gates is an evaluated event: pre-effect inputs are recorded
//!   **before** `invoke` (or at the gate's denial point), the terminal
//!   observation is recorded at the event's end, and the ER is projected from
//!   those records at the terminal point — including for events whose round
//!   will never commit (denial after an earlier successful tool, timeout,
//!   scan rejection), which the round-level mirror cannot cover.
//! - **Replay is idempotent and never re-executes.** At open, the emitter
//!   reads the evidence journal and re-projects every terminal event whose
//!   `step_id` (the stable event id) is not already in the ER chain — crash
//!   recovery for the window between an event's durable record and its ER
//!   append. An event stranded pre-observation (crash mid-invoke, dropped
//!   stream) projects an explicit `insufficient_evidence` ER; the tool is
//!   never re-run (the emitter has no tool-registry access at all).
//! - **The journal is checked, not trusted.** A torn journal tail, a post
//!   record without its pre, a duplicate record, or arguments that do not
//!   hash to the recorded digests fails the open — mirroring never chains
//!   onto evidence it cannot account for.
//! - **Round mirror semantics are unchanged.** One ER per committed round at
//!   the commit decision; abandoned / cancelled turns mint no round ER; the
//!   terminal cancellation marker is not an event and mints no ER either.
//! - **A mirror gap is honest absence.** Any projection/append failure
//!   poisons the emitter for its lifetime so later ERs cannot silently skip
//!   the gap: the missing ER reads downstream as `insufficient_evidence`,
//!   never as compliance.
//! - **Single writer.** One emitter owns one mirror log AND its evidence
//!   journal (same data dir). Two live emitters over the same pair — e.g.
//!   two CLI processes sharing `~/.ardur` — trip the fork guard, and a
//!   survivor's open would treat the victim's in-flight events as
//!   crash-stranded. Bind one mirror per data dir, exactly as #560 specified
//!   for the chain alone.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ardur_governance::{
    ActionClass, AuthOutcome, ErRoundFacts, ErSigner, ErSigningKey, EvidenceLevel, EvidenceRecord,
    GovernanceEmitter, PostEffectRecord, PreEffectRecord, SideEffectClass, SignedExecutionReceipt,
    StepContext, ToolInvocation, project_event_execution_receipt, project_execution_receipt,
    verify_er_log_lines,
};
use ardur_receipt::Es256SigningKey;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::receipts::{open_append_no_follow, open_regular_no_follow};

/// The ER `resource_family` for a mirrored chat round.
const CHAT_RESOURCE_FAMILY: &str = "llm_completion";
/// ER lifetime (`exp = iat + ttl`), matching the DESIGN.md CR-5 default.
const ER_TTL_SECS: u64 = 300;
/// The evidence journal file name, beside the ER chain log.
const EVENTS_FILE_NAME: &str = "events.jsonl";

/// The emitter's locked state: the ER chain tail and fork-guard length, the
/// evidence journal's fork-guard length, the dedup sets, and the poison
/// marker. One lock serializes every append so a file-backed mirror chains
/// and journals without forking (the runtime calls from the commit lock for
/// rounds and from the tool loop for events).
struct EmitterState {
    /// The last chained ER (the next ER's parent).
    tail: Option<SignedExecutionReceipt>,
    /// The on-disk chain length after the last successful append. Each append
    /// first checks the log still ends exactly there: if a foreign writer (a
    /// second emitter over the same path, a log rotator) has changed the
    /// file, our cached tail is stale and appending would fork the chain —
    /// so the append fails closed instead of racing it.
    chain_committed_len: u64,
    /// The same fork-guard for the evidence journal.
    events_committed_len: u64,
    /// The next evidence-journal sequence number, and the chain MAC of the
    /// journal's last line (empty when the journal is empty). Every append
    /// links its MAC to this tail, so a deleted line breaks the chain and a
    /// truncated tail contradicts the authenticated checkpoint.
    next_seq: u64,
    tail_chain_mac: String,
    /// The `step_id`s of every ER in the chain — event ids (`ev:…`) for
    /// evaluated events, native receipt ids for committed rounds. The
    /// idempotency key for both live re-mirroring and the open-time sweep.
    chained_step_ids: HashSet<String>,
    /// Events with a durable pre-effect record but no post yet (in-flight).
    open_event_ids: HashSet<String>,
    /// Events with a durable post-effect record (terminal).
    closed_event_ids: HashSet<String>,
    /// Set when an append's outcome is ambiguous (the line may be on disk
    /// while the tail was not advanced) or a projection failed. Once
    /// poisoned, every later mirror fails fast: writing a fresh chained line
    /// onto a possibly-present predecessor would manufacture a broken chain
    /// out of uncertainty.
    poisoned: Option<String>,
}

/// A file-backed [`GovernanceEmitter`] mirroring committed rounds and
/// evaluated events into a hash-chained ER log signed with the runtime's own
/// receipt custody, with the per-event evidence journal beside it.
///
/// [`open`](Self::open) verifies any existing mirror log against the key
/// before seeding the chain tail, so a restart resumes the chain instead of
/// forking it — the same boot contract the native receipt log follows. It
/// then replays the evidence journal: every terminal event without a chained
/// ER is projected and appended, idempotently. One emitter owns one log;
/// concurrent writers are excluded by the same single-writer discipline as
/// the native receipt log (bind one mirror per receipt log / data dir).
pub struct ErMirrorEmitter {
    path: PathBuf,
    events_path: PathBuf,
    key: ErSigningKey,
    /// The journal-line MAC key, derived from the ER signing key under a
    /// domain-separated label (#543 review: stranded records must be
    /// unforgeable in the crash window between append and mirror).
    events_mac_key: [u8; 32],
    verifier_id: String,
    run_nonce: String,
    inner: Mutex<EmitterState>,
}

impl ErMirrorEmitter {
    /// Open (or create) the mirror log at `path`, signing with the same
    /// P-256 custody as the native receipts (`receipt_key`) and stamping
    /// `verifier_id` on every ER. The evidence journal is the sibling
    /// `events.jsonl` in the same directory.
    ///
    /// An existing log is fully verified (signatures then linkage) before the
    /// tail is seeded; a corrupt or foreign-key log fails the open rather
    /// than silently re-genesis. An existing evidence journal is checked
    /// (torn tail, dangling post, duplicate record all fail) and swept:
    /// terminal events without a chained ER are projected and appended in
    /// journal order.
    ///
    /// # Errors
    ///
    /// [`ardur_governance::GovernanceError::Key`] if the custody cannot be
    /// shared, `Verify`/`BrokenChain` if the existing log does not verify
    /// against it, `InvalidClaim` if a journaled event cannot be projected
    /// honestly (including recorded arguments that do not hash to the
    /// recorded digests), or `Io` if either file cannot be read or created
    /// or is structurally inconsistent.
    pub fn open(
        path: &Path,
        receipt_key: &Es256SigningKey,
        verifier_id: &str,
    ) -> Result<Self, ardur_governance::GovernanceError> {
        let pem = receipt_key
            .to_pkcs8_pem()
            .map_err(|e| ardur_governance::GovernanceError::Key(format!("pem: {e}")))?;
        let key = ErSigningKey::from_pkcs8_pem(&pem)?;
        let events_mac_key = ardur_governance::evidence_record_mac_key(pem.as_bytes());
        let run_nonce = run_nonce();

        // The hardened descriptor-relative open needs an absolute path; a
        // relative mirror path is resolved against the cwd (the same
        // normalization the runtime builder applies to receipt logs).
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| ardur_governance::GovernanceError::Io(format!("cwd: {e}")))?
                .join(path)
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ardur_governance::GovernanceError::Io(format!("mkdir: {e}")))?;
        }
        // Establish (and permission) the file with the same hardened open the
        // native receipt log uses, then fsync the PARENT directory too:
        // creating the log (and the `governance/` directory itself) must be
        // durable, and a file-only fsync does not make a directory entry
        // survive a power loss.
        let file = open_append_no_follow(&path)
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("open: {e}")))?;
        file.sync_all()
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("fsync: {e}")))?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)
                .and_then(|d| d.sync_all())
                .map_err(|e| ardur_governance::GovernanceError::Io(format!("parent fsync: {e}")))?;
        }
        let chain_committed_len = file
            .metadata()
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("stat: {e}")))?
            .len();

        // Seed the tail by reading through a no-follow READ descriptor —
        // a plain path-based re-read could follow a symlink swapped in after
        // the hardened open (TOCTOU) and seed the tail from a different log.
        let reader = open_regular_no_follow(&path, false)
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("read open: {e}")))?;
        let existing = read_all_from(&reader)
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("read: {e}")))?;
        // A torn tail (a complete JWS missing its trailing newline, the
        // residue of a crash mid-append) is ambiguous state: `str::lines`
        // would accept and verify the record, and the next append would then
        // concatenate onto it, permanently corrupting the log. Fail closed.
        if !existing.is_empty() && existing.last() != Some(&b'\n') {
            return Err(ardur_governance::GovernanceError::Io(
                "mirror log tail is torn (last line lacks its newline); refusing to chain"
                    .to_string(),
            ));
        }
        // Rebuild the chain through the CHECKED path: claims are decoded from
        // each verified JWS itself, then the linkage is checked.
        let lines: Vec<String> = std::str::from_utf8(&existing)
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("utf8: {e}")))?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect();
        let chain = verify_er_log_lines(&lines, &key.jwks())?;
        let chained_step_ids: HashSet<String> = chain
            .iter()
            .map(|er| er.receipt().step_id.clone())
            .collect();

        // ---- #543: open the evidence journal, check it, and sweep. ----
        // The events file gets the same durability discipline as the chain
        // log: fsync the file itself, then fsync the PARENT again — the
        // earlier parent fsync covered the chain log's directory entry, not
        // `events.jsonl`'s, which may have been created just now.
        let events_path = path.with_file_name(EVENTS_FILE_NAME);
        let events_file = open_append_no_follow(&events_path)
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("events open: {e}")))?;
        events_file
            .sync_all()
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("events fsync: {e}")))?;
        if let Some(parent) = events_path.parent() {
            std::fs::File::open(parent)
                .and_then(|d| d.sync_all())
                .map_err(|e| {
                    ardur_governance::GovernanceError::Io(format!("events parent fsync: {e}"))
                })?;
        }
        let events_committed_len = events_file
            .metadata()
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("events stat: {e}")))?
            .len();
        let events_reader = open_regular_no_follow(&events_path, false)
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("events read open: {e}")))?;
        let events_bytes = read_all_from(&events_reader)
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("events read: {e}")))?;
        let (events, next_seq, tail_chain_mac) =
            parse_evidence_journal(&events_bytes, &events_mac_key)?;

        // ---- Completeness: the journal must end where it last committed. ----
        // The chain MACs prove linkage (a deleted line anywhere breaks them);
        // the authenticated tail checkpoint proves the journal still ends
        // where the emitter last fsynced it — otherwise a crash-window
        // editor could delete a stranded pair (or just its post, downgrading
        // a completed event to `effect_unobserved`) and the per-line MACs
        // would still verify. An empty journal with a checkpoint is a
        // wholesale deletion; a non-empty journal without one (or with a
        // mismatching one) is a tail truncation. Both fail closed.
        let checkpoint_path = events_path.with_file_name("events.tail");
        if next_seq == 0 {
            if checkpoint_path.symlink_metadata().is_ok() {
                return Err(ardur_governance::GovernanceError::Io(
                    "evidence journal is empty but an authenticated tail checkpoint exists; \
                     the journal was truncated or deleted after events were recorded"
                        .to_string(),
                ));
            }
        } else {
            let checkpoint_file = open_regular_no_follow(&checkpoint_path, false).map_err(|e| {
                ardur_governance::GovernanceError::Io(format!(
                    "evidence journal tail checkpoint is unreadable ({e}); the journal tail \
                     may have been truncated — refusing to replay"
                ))
            })?;
            let checkpoint_bytes = read_all_from(&checkpoint_file).map_err(|e| {
                ardur_governance::GovernanceError::Io(format!("events checkpoint read: {e}"))
            })?;
            let checkpoint_json = std::str::from_utf8(&checkpoint_bytes).map_err(|e| {
                ardur_governance::GovernanceError::Io(format!("checkpoint utf8: {e}"))
            })?;
            let (checkpoint_seq, checkpoint_tail) = ardur_governance::verify_evidence_checkpoint(
                &events_mac_key,
                checkpoint_json.trim(),
            )?;
            if checkpoint_seq != next_seq - 1 || checkpoint_tail != tail_chain_mac {
                return Err(ardur_governance::GovernanceError::Io(
                    "evidence journal tail does not match its authenticated checkpoint \
                     (tail truncation); refusing to replay"
                        .to_string(),
                ));
            }
        }

        let mut state = EmitterState {
            tail: chain.last().cloned(),
            chain_committed_len,
            events_committed_len,
            next_seq,
            tail_chain_mac,
            chained_step_ids,
            open_event_ids: HashSet::new(),
            closed_event_ids: HashSet::new(),
            poisoned: None,
        };

        // ---- #543: reconcile the chain and the journal bidirectionally. ----
        // An already-chained event is NOT trusted because it is chained, and
        // a missing journal is not trusted because it parses: for every
        // chained event ER the journaled records must exist (a journal
        // deleted or truncated after mirroring fails closed) AND must
        // re-project — with their original parent — to the chained receipt
        // exactly (editing a mirrored event's recorded outcome or arguments
        // fails closed), even though no new ER needs appending.
        //
        // The lookup index is built once: both files are append-only and
        // unbounded, so a per-event linear scan would make every restart
        // quadratic in the lifetime event count.
        let events_by_id: std::collections::HashMap<
            &str,
            &(PreEffectRecord, Option<PostEffectRecord>),
        > = events
            .iter()
            .map(|pair| (pair.0.event_id.as_str(), pair))
            .collect();
        for (idx, er) in chain.iter().enumerate() {
            let step_id = &er.receipt().step_id;
            if !step_id.starts_with("ev:") {
                continue;
            }
            let Some((pre, post)) = events_by_id.get(step_id.as_str()).copied() else {
                return Err(ardur_governance::GovernanceError::Io(format!(
                    "chained event ER {step_id} has no durable evidence record; refusing \
                     to mirror over missing evidence"
                )));
            };
            let parent = idx.checked_sub(1).map(|p| &chain[p]);
            let projected = project_event_execution_receipt(
                pre,
                post.as_ref(),
                verifier_id,
                ER_TTL_SECS,
                parent,
            )?;
            if &projected != er.receipt() {
                return Err(ardur_governance::GovernanceError::Io(format!(
                    "chained event ER {step_id} does not reproduce from its journaled \
                     evidence (tampered journal); refusing to mirror"
                )));
            }
        }

        // The crash-recovery sweep: every journaled event the chain does not
        // yet carry is projected from its durable records — a terminal event
        // from its pre+post pair, a stranded event (pre only) as an explicit
        // `insufficient_evidence` orphan. Appends go through the same
        // fork-guarded helper as live mirroring; any failure fails the open
        // so the operator reconciles instead of the mirror silently skipping
        // an event. A later open resumes idempotently: already-chained event
        // ids are skipped, and the sweep never mutates the journal itself.
        for (pre, post) in &events {
            if state.chained_step_ids.contains(&pre.event_id) {
                mark_event_recorded(&mut state, pre, post.as_ref());
                continue;
            }
            let receipt = project_event_execution_receipt(
                pre,
                post.as_ref(),
                verifier_id,
                ER_TTL_SECS,
                state.tail.as_ref(),
            )?;
            let signed = ErSigner::sign(receipt, &key)?;
            append_chain_line(&path, &mut state, signed)?;
            mark_event_recorded(&mut state, pre, post.as_ref());
        }

        Ok(Self {
            path,
            events_path,
            key,
            events_mac_key,
            verifier_id: verifier_id.to_string(),
            run_nonce,
            inner: Mutex::new(state),
        })
    }

    /// Open the mirror at the #502 Seam B7 convention:
    /// `<data_dir>/governance/er-chain.jsonl` (the `$ARDUR_DATA_DIR`
    /// layout), with the evidence journal at
    /// `<data_dir>/governance/events.jsonl`. See [`open`](Self::open) for the
    /// custody, verification, and sweep contract.
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open).
    pub fn open_in_data_dir(
        data_dir: &Path,
        receipt_key: &Es256SigningKey,
        verifier_id: &str,
    ) -> Result<Self, ardur_governance::GovernanceError> {
        Self::open(
            &data_dir.join("governance").join("er-chain.jsonl"),
            receipt_key,
            verifier_id,
        )
    }

    /// The mirror log path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The evidence journal path (#543).
    #[must_use]
    pub fn events_path(&self) -> &Path {
        &self.events_path
    }
}

impl GovernanceEmitter for ErMirrorEmitter {
    fn mirror_committed_round(
        &self,
        facts: &ErRoundFacts<'_>,
    ) -> Result<(), ardur_governance::GovernanceError> {
        // A previous failure poisons this emitter for its lifetime. Any
        // error — ambiguous append OR projection failure — means a committed
        // native round has no ER: minting later ERs would produce a fully
        // verifiable mirror that silently skips it, which reads downstream
        // as continuous compliance instead of the honest gap. Stopping makes
        // the gap observable (mirror count < native count, every later round
        // logs a mirror failure); the operator re-opens — which re-verifies
        // the on-disk log — after reconciling the omission.
        let mut state = self.inner.lock();
        if let Some(reason) = state.poisoned.as_deref() {
            return Err(poisoned_error(reason));
        }

        // The invocation the ER digests: the round's admission identity plus
        // the tool-call digests the native receipt already recorded. Nothing
        // here is reconstructed or re-derived — it is the same signed-body
        // fact set as the native receipt.
        let arguments = round_arguments(facts);
        let invocation = ToolInvocation {
            tool: facts.tool,
            action_class: ActionClass::Summarize,
            target: facts.provider,
            resource_family: CHAT_RESOURCE_FAMILY,
            // Keyed off actual persistence, not tool count: a no-tool round
            // over a journaling runtime still wrote transcript state
            // (`InternalWrite`), while a tool round over a journal-less
            // runtime changed nothing durable (`None`). Over-claiming here
            // would make governance evidence depend on the wrong fact.
            side_effect_class: if facts.persisted_transcript {
                SideEffectClass::InternalWrite
            } else {
                SideEffectClass::None
            },
            arguments: &arguments,
        };

        let receipt = project_execution_receipt(
            facts.claims,
            &invocation,
            &AuthOutcome::Compliant,
            &StepContext {
                verifier_id: &self.verifier_id,
                iss: &self.verifier_id,
                trace_id: facts.trace_id,
                run_nonce: &self.run_nonce,
                step_id: facts.step_id,
                timestamp_millis: facts.timestamp_millis,
                ttl_secs: ER_TTL_SECS,
                evidence_level: EvidenceLevel::SelfSigned,
                parent: state.tail.as_ref(),
                // Phase 1 keeps the legacy economic `cost` scalar: the native
                // side maintains no per-effect-class budget yet, and the
                // registry forbids inventing zero-filled classes (#545).
                per_class_budget_remaining: None,
            },
        );
        let receipt = match receipt {
            Ok(receipt) => receipt,
            Err(e) => {
                // A projection failure is also a gap: poison so later rounds
                // cannot silently skip past it (see the guard at the top).
                state.poisoned = Some(e.to_string());
                return Err(e);
            }
        };
        let signed = match ErSigner::sign(receipt, &self.key) {
            Ok(signed) => signed,
            Err(e) => {
                state.poisoned = Some(e.to_string());
                return Err(e);
            }
        };
        append_chain_line(&self.path, &mut state, signed)
    }

    fn record_pre_effect(
        &self,
        record: &PreEffectRecord,
    ) -> Result<(), ardur_governance::GovernanceError> {
        let mut state = self.inner.lock();
        if let Some(reason) = state.poisoned.as_deref() {
            return Err(poisoned_error(reason));
        }
        if state.open_event_ids.contains(&record.event_id)
            || state.closed_event_ids.contains(&record.event_id)
        {
            let err = ardur_governance::GovernanceError::Io(format!(
                "duplicate pre-effect record for event {}; the journal would no longer replay",
                record.event_id
            ));
            state.poisoned = Some(err.to_string());
            return Err(err);
        }
        let (line, chain_mac) = ardur_governance::wrap_evidence_line(
            &self.events_mac_key,
            state.next_seq,
            &state.tail_chain_mac,
            &EvidenceRecord::PreEffect(Box::new(record.clone())).to_line()?,
        );
        append_events_line(&self.events_path, &mut state, &line)?;
        if let Err(e) = write_events_checkpoint(
            &self.events_path,
            &self.events_mac_key,
            state.next_seq,
            &chain_mac,
        ) {
            state.poisoned = Some(e.to_string());
            return Err(e);
        }
        state.tail_chain_mac = chain_mac;
        state.next_seq += 1;
        state.open_event_ids.insert(record.event_id.clone());
        Ok(())
    }

    fn record_post_effect(
        &self,
        record: &PostEffectRecord,
    ) -> Result<(), ardur_governance::GovernanceError> {
        let mut state = self.inner.lock();
        if let Some(reason) = state.poisoned.as_deref() {
            return Err(poisoned_error(reason));
        }
        if !state.open_event_ids.contains(&record.event_id) {
            let err = ardur_governance::GovernanceError::Io(format!(
                "post-effect record for event {} has no pre-effect record; the journal would \
                 no longer replay",
                record.event_id
            ));
            state.poisoned = Some(err.to_string());
            return Err(err);
        }
        if state.closed_event_ids.contains(&record.event_id) {
            let err = ardur_governance::GovernanceError::Io(format!(
                "duplicate post-effect record for event {}",
                record.event_id
            ));
            state.poisoned = Some(err.to_string());
            return Err(err);
        }
        let (line, chain_mac) = ardur_governance::wrap_evidence_line(
            &self.events_mac_key,
            state.next_seq,
            &state.tail_chain_mac,
            &EvidenceRecord::PostEffect(record.clone()).to_line()?,
        );
        append_events_line(&self.events_path, &mut state, &line)?;
        if let Err(e) = write_events_checkpoint(
            &self.events_path,
            &self.events_mac_key,
            state.next_seq,
            &chain_mac,
        ) {
            state.poisoned = Some(e.to_string());
            return Err(e);
        }
        state.tail_chain_mac = chain_mac;
        state.next_seq += 1;
        state.open_event_ids.remove(&record.event_id);
        state.closed_event_ids.insert(record.event_id.clone());
        Ok(())
    }

    fn mirror_evaluated_event(
        &self,
        pre: &PreEffectRecord,
        post: &PostEffectRecord,
    ) -> Result<(), ardur_governance::GovernanceError> {
        let mut state = self.inner.lock();
        if let Some(reason) = state.poisoned.as_deref() {
            return Err(poisoned_error(reason));
        }
        // Idempotent per event: a replayed terminal (a live mirror racing a
        // boot sweep, or a caller retry) must not duplicate the ER.
        if state.chained_step_ids.contains(&pre.event_id) {
            return Ok(());
        }
        let receipt = match project_event_execution_receipt(
            pre,
            Some(post),
            &self.verifier_id,
            ER_TTL_SECS,
            state.tail.as_ref(),
        ) {
            Ok(receipt) => receipt,
            Err(e) => {
                state.poisoned = Some(e.to_string());
                return Err(e);
            }
        };
        let signed = match ErSigner::sign(receipt, &self.key) {
            Ok(signed) => signed,
            Err(e) => {
                state.poisoned = Some(e.to_string());
                return Err(e);
            }
        };
        append_chain_line(&self.path, &mut state, signed)
    }
}

/// The poison guard's shared error shape.
fn poisoned_error(reason: &str) -> ardur_governance::GovernanceError {
    ardur_governance::GovernanceError::Io(format!(
        "mirror poisoned by an earlier failure: {reason}"
    ))
}

/// Record an event's presence in the dedup sets after a sweep append (or when
/// the sweep finds it already chained).
fn mark_event_recorded(
    state: &mut EmitterState,
    pre: &PreEffectRecord,
    post: Option<&PostEffectRecord>,
) {
    if post.is_some() {
        state.closed_event_ids.insert(pre.event_id.clone());
    } else {
        state.open_event_ids.insert(pre.event_id.clone());
    }
}

/// Append one signed ER to the chain log with the fork guard, advancing the
/// tail and the dedup set. An ambiguous write (error after the line may have
/// reached disk) poisons the emitter rather than advancing the tail, so no
/// later line can chain onto a state we are not sure of.
fn append_chain_line(
    path: &Path,
    state: &mut EmitterState,
    signed: SignedExecutionReceipt,
) -> Result<(), ardur_governance::GovernanceError> {
    use std::io::Write as _;
    let step_id = signed.receipt().step_id.clone();
    // ANY failure here poisons the emitter, not only write/fsync errors: a
    // failed event-ER append that left the emitter live would let later
    // receipts chain past the omitted evaluated event — a fully verifiable
    // chain that silently skips it, reading downstream as continuous
    // compliance instead of the honest gap (the same argument the round
    // mirror's poison guard documents). The operator re-opens — which
    // re-verifies and sweeps — after reconciling the omission.
    let mut file = match open_append_no_follow(path) {
        Ok(file) => file,
        Err(e) => {
            let err = ardur_governance::GovernanceError::Io(format!("append open: {e}"));
            state.poisoned = Some(err.to_string());
            return Err(err);
        }
    };
    // Fork guard: the log must still end exactly where our last successful
    // append left it. A concurrent writer (a second emitter over the same
    // path) or an external truncation means the cached tail is stale —
    // appending would fork the chain, so fail closed.
    let actual = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(e) => {
            let err = ardur_governance::GovernanceError::Io(format!("stat: {e}"));
            state.poisoned = Some(err.to_string());
            return Err(err);
        }
    };
    if actual != state.chain_committed_len {
        let err = ardur_governance::GovernanceError::Io(format!(
            "mirror log changed under us (len {actual} != committed {}): one emitter must own \
             one mirror log",
            state.chain_committed_len
        ));
        state.poisoned = Some(err.to_string());
        return Err(err);
    }
    let append = writeln!(file, "{}", signed.jws_compact()).and_then(|()| file.sync_all());
    match append {
        Ok(()) => {
            state.chain_committed_len = actual + signed.jws_compact().len() as u64 + 1;
        }
        Err(e) => {
            state.poisoned = Some(e.to_string());
            return Err(ardur_governance::GovernanceError::Io(format!(
                "append: {e}"
            )));
        }
    }
    state.chained_step_ids.insert(step_id);
    state.tail = Some(signed);
    Ok(())
}

/// Create+truncate the checkpoint with the same no-follow discipline as the
/// journal and receipt chain.
#[cfg(unix)]
fn open_checkpoint_truncate(checkpoint_path: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
    let parent = checkpoint_path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent"))?;
    let parent = openat(
        rustix::fs::CWD,
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
    let name = checkpoint_path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "checkpoint has no file name",
        )
    })?;
    openat(
        &parent,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map(std::fs::File::from)
    .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))
}

/// Create+truncate the checkpoint (non-unix fallback).
#[cfg(not(unix))]
fn open_checkpoint_truncate(checkpoint_path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(checkpoint_path)
}

/// Rewrite the authenticated tail checkpoint beside the journal with the
/// same no-follow + fsync discipline as the journal itself. The checkpoint
/// is what makes a TAIL truncation as detectable as a mid-journal deletion:
/// the chain MACs alone prove linkage, and this proves the journal still
/// ends where the emitter last committed it. Failure poisons the caller the
/// same way an append failure does (handled at the call site).
fn write_events_checkpoint(
    events_path: &Path,
    key: &[u8; 32],
    seq: u64,
    tail_chain_mac: &str,
) -> Result<(), ardur_governance::GovernanceError> {
    use std::io::Write as _;
    let checkpoint_path = events_path.with_file_name("events.tail");
    let json = ardur_governance::evidence_checkpoint_json(key, seq, tail_chain_mac);
    let mut file = open_checkpoint_truncate(&checkpoint_path).map_err(|e| {
        ardur_governance::GovernanceError::Io(format!("events checkpoint open: {e}"))
    })?;
    file.write_all(json.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            // The receipt-chain discipline: fsync the directory entry too, so
            // the checkpoint survives a crash that preserves the file.
            std::fs::File::open(checkpoint_path.parent().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "no checkpoint parent")
            })?)?
            .sync_all()
        })
        .map_err(|e| ardur_governance::GovernanceError::Io(format!("events checkpoint write: {e}")))
}

/// Append one evidence record line to the journal with the same fork guard
/// and fsync discipline as the chain. An ambiguous write poisons the emitter:
/// a possibly-missing record would silently degrade crash recovery to
/// absence otherwise, which the gap-observability obligation forbids.
fn append_events_line(
    path: &Path,
    state: &mut EmitterState,
    line: &str,
) -> Result<(), ardur_governance::GovernanceError> {
    use std::io::Write as _;
    // As with the chain append, EVERY failure here poisons the emitter: a
    // journal that cannot be appended to is exactly the evidence gap the
    // fail-closed posture exists to surface — letting rounds continue past
    // an event whose evidence cannot be journaled would hide it.
    let mut file = match open_append_no_follow(path) {
        Ok(file) => file,
        Err(e) => {
            let err = ardur_governance::GovernanceError::Io(format!("events append open: {e}"));
            state.poisoned = Some(err.to_string());
            return Err(err);
        }
    };
    let actual = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(e) => {
            let err = ardur_governance::GovernanceError::Io(format!("events stat: {e}"));
            state.poisoned = Some(err.to_string());
            return Err(err);
        }
    };
    if actual != state.events_committed_len {
        let err = ardur_governance::GovernanceError::Io(format!(
            "evidence journal changed under us (len {actual} != committed {}): one emitter \
             must own one journal",
            state.events_committed_len
        ));
        state.poisoned = Some(err.to_string());
        return Err(err);
    }
    let append = writeln!(file, "{line}").and_then(|()| file.sync_all());
    match append {
        Ok(()) => {
            state.events_committed_len = actual + line.len() as u64 + 1;
            Ok(())
        }
        Err(e) => {
            state.poisoned = Some(e.to_string());
            Err(ardur_governance::GovernanceError::Io(format!(
                "events append: {e}"
            )))
        }
    }
}

/// The parsed journal: the paired records, the next sequence number, and the
/// verified chain MAC of the last line (empty when the journal is empty).
type ParsedEvidenceJournal = (
    Vec<(PreEffectRecord, Option<PostEffectRecord>)>,
    u64,
    String,
);

/// Parse the evidence journal into ordered (pre, post) pairs. Structural
/// inconsistencies fail the open: a torn tail (crash mid-append residue —
/// appending past it would corrupt the journal), an unparseable line, a
/// duplicate pre/post, or a post without its pre. Anything less strict would
/// let the mirror chain onto evidence it cannot account for.
fn parse_evidence_journal(
    bytes: &[u8],
    mac_key: &[u8; 32],
) -> Result<ParsedEvidenceJournal, ardur_governance::GovernanceError> {
    use std::collections::HashMap;
    if bytes.is_empty() {
        return Ok((Vec::new(), 0, String::new()));
    }
    if bytes.last() != Some(&b'\n') {
        return Err(ardur_governance::GovernanceError::Io(
            "evidence journal tail is torn (last line lacks its newline); refusing to replay"
                .to_string(),
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ardur_governance::GovernanceError::Io(format!("events utf8: {e}")))?;
    let mut pres: Vec<PreEffectRecord> = Vec::new();
    let mut pre_index: HashMap<String, usize> = HashMap::new();
    let mut posts: HashMap<String, PostEffectRecord> = HashMap::new();
    // The chain walk: each line's MAC must cover its sequence number and the
    // previous line's chain MAC, so deleting ANY line (not just editing one)
    // breaks the linkage — per-line MACs alone authenticate each line
    // independently and cannot detect one going missing.
    let mut prev_chain_mac = String::new();
    for (i, line) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let seq = i as u64;
        // Verify the keyed chain-MAC BEFORE any semantic check: without it an
        // editor of the crash window could falsify a stranded record's
        // provenance AND its publicly recomputable identity, and the sweep
        // would sign the forgery.
        let (line, verified_mac) =
            ardur_governance::unwrap_evidence_line(mac_key, seq, &prev_chain_mac, line).map_err(
                |e| {
                    ardur_governance::GovernanceError::Io(format!("evidence journal line {i}: {e}"))
                },
            )?;
        prev_chain_mac = verified_mac;
        let record = EvidenceRecord::from_line(&line).map_err(|e| {
            ardur_governance::GovernanceError::Io(format!("evidence journal line {i}: {e}"))
        })?;
        match record {
            EvidenceRecord::PreEffect(pre) => {
                let pre = *pre;
                if pre_index.contains_key(&pre.event_id) || posts.contains_key(&pre.event_id) {
                    return Err(ardur_governance::GovernanceError::Io(format!(
                        "evidence journal line {i}: duplicate pre-effect record for event {}",
                        pre.event_id
                    )));
                }
                pre_index.insert(pre.event_id.clone(), pres.len());
                pres.push(pre);
            }
            EvidenceRecord::PostEffect(post) => {
                if !pre_index.contains_key(&post.event_id) {
                    return Err(ardur_governance::GovernanceError::Io(format!(
                        "evidence journal line {i}: post-effect record for event {} has no \
                         pre-effect record",
                        post.event_id
                    )));
                }
                if posts.contains_key(&post.event_id) {
                    return Err(ardur_governance::GovernanceError::Io(format!(
                        "evidence journal line {i}: duplicate post-effect record for event {}",
                        post.event_id
                    )));
                }
                posts.insert(post.event_id.clone(), post);
            }
        }
    }
    let next_seq = pres.len() as u64 + posts.len() as u64;
    Ok((
        pres.into_iter()
            .map(|pre| {
                let post = posts.get(&pre.event_id).cloned();
                (pre, post)
            })
            .collect(),
        next_seq,
        prev_chain_mac,
    ))
}

/// Read the whole file through an already-open descriptor (no path re-open,
/// so no symlink-swap TOCTOU between the hardened open and the read).
fn read_all_from(file: &std::fs::File) -> std::io::Result<Vec<u8>> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = file;
    file.seek(SeekFrom::Start(0))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// The JCS-hashed argument envelope: the committed round's identity plus its
/// recorded tool calls as digests.
fn round_arguments(facts: &ErRoundFacts<'_>) -> Value {
    json!({
        "capability": facts.tool,
        "native_receipt_id": facts.step_id,
        "provider": facts.provider,
        "tool_calls": facts
            .tool_calls
            .iter()
            .map(|tc| {
                json!({
                    "call_id": tc.call_id,
                    "tool": tc.tool_name,
                    "arguments_sha256": tc.arguments_digest,
                    "output_sha256": tc.output_digest,
                })
            })
            .collect::<Vec<_>>(),
    })
}

/// A fresh per-emitter (per-process) run nonce: 16 CSPRNG bytes, base64url.
fn run_nonce() -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let uuid = uuid::Uuid::new_v4();
    URL_SAFE_NO_PAD.encode(uuid.as_bytes())
}
