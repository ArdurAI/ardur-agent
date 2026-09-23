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

/// Basenames the emitter derives as siblings of the chain log. A mirror path
/// whose own basename collides with any of these would alias the chain onto
/// the journal (or have a sibling publish rename over the chain) — rejected
/// at open instead of failing later on the fork guard or a JWS parse of a
/// MAC envelope.
const RESERVED_SIBLING_BASENAMES: &[&str] = &[
    "events.jsonl",
    "events.tail",
    "events.anchor",
    "events.lock",
    "events.tail.tmp",
    "events.anchor.tmp",
];

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
    /// The records actually journaled, retained so `mirror_evaluated_event`
    /// can require the caller-supplied records to be EXACTLY these — an ER
    /// must never claim evidence that was never durable. Seeded from the
    /// verified parse at open; updated on each successful record append.
    journaled: std::collections::HashMap<String, (PreEffectRecord, Option<PostEffectRecord>)>,
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
    /// The exclusive journal-ownership lock: held for the emitter's
    /// lifetime so a second runtime over the same data directory cannot
    /// interleave appends (the fork check would otherwise be a TOCTOU
    /// against its own metadata read). The lock is on the INODE: the path is
    /// re-checked against it before every transaction, because an editor
    /// could unlink and recreate `events.lock` so a second emitter locks a
    /// different inode.
    events_lock: std::fs::File,
    events_lock_path: PathBuf,
    /// The (dev, ino) identities of the chain log and the evidence journal
    /// as opened. Both append paths reopen by pathname; the identity check
    /// at open protects only startup unless every reopened descriptor is
    /// re-verified against these — a directory writer could otherwise
    /// replace `events.jsonl` with a hardlink to the chain after open and
    /// the next pre-effect append would write a MAC envelope into the ER
    /// log.
    chain_identity: (u64, u64),
    events_identity: (u64, u64),
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
        if let Some(basename) = path.file_name().and_then(|n| n.to_str()) {
            if RESERVED_SIBLING_BASENAMES.contains(&basename) {
                return Err(ardur_governance::GovernanceError::Io(format!(
                    "mirror path basename `{basename}` collides with an evidence sibling \
                     name; the ER chain and the evidence journal must be distinct files"
                )));
            }
        }
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

        // ---- Exclusive journal ownership (one emitter per data directory). ----
        // Two runtimes sharing one directory could otherwise both pass the
        // length fork-check before either writes (O_APPEND serializes the
        // writes, not the check/write transaction), emitting duplicate-seq
        // envelopes that fail chain verification on restart. Hold an
        // exclusive advisory lock for the emitter's whole lifetime: the
        // second opener fails immediately instead of corrupting the journal,
        // and the kernel releases the lock if the holder dies.
        let lock_path = events_path.with_file_name("events.lock");
        let events_lock = open_append_no_follow(&lock_path)
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("events lock open: {e}")))?;
        #[cfg(unix)]
        rustix::fs::flock(
            &events_lock,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        )
        .map_err(|e| {
            ardur_governance::GovernanceError::Io(format!(
                "events lock: another emitter owns this evidence journal ({e})"
            ))
        })?;
        // The workspace MSRV (1.85) predates std's portable file locks, so
        // non-unix hosts have no exclusive-ownership primitive here: holding
        // the lock file open would provide NO exclusion and two emitters
        // could race the fork-check/append transaction. Fail closed rather
        // than run the mirror without its ownership guard.
        #[cfg(not(unix))]
        return Err(ardur_governance::GovernanceError::Io(
            "the evidence journal's exclusive-ownership guard requires a unix host; \
             refusing to start the mirror without it"
                .to_string(),
        ));

        let events_file = open_append_no_follow(&events_path)
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("events open: {e}")))?;
        // The basename check is lexical: on a case-insensitive volume
        // `EVENTS.JSONL` aliases `events.jsonl`, and a differently named
        // mirror path can be pre-hardlinked to it. Compare the OPENED files'
        // identities — matching device/inode pairs mean the chain and the
        // journal are the same physical file, and the mirror must refuse to
        // start rather than append MAC envelopes into the ER log.
        #[cfg(unix)]
        let (chain_identity, events_identity) = {
            use std::os::unix::fs::MetadataExt as _;
            let chain_meta = file
                .metadata()
                .map_err(|e| ardur_governance::GovernanceError::Io(format!("chain stat: {e}")))?;
            let events_meta = events_file
                .metadata()
                .map_err(|e| ardur_governance::GovernanceError::Io(format!("events stat: {e}")))?;
            let identities = (
                (chain_meta.dev(), chain_meta.ino()),
                (events_meta.dev(), events_meta.ino()),
            );
            if chain_meta.dev() == events_meta.dev() && chain_meta.ino() == events_meta.ino() {
                return Err(ardur_governance::GovernanceError::Io(
                    "the mirror chain and the evidence journal are the same physical file                      (case-insensitive alias or hardlink); refusing to open"
                        .to_string(),
                ));
            }
            identities
        };
        // Non-unix fails closed at the ownership guard above before any
        // append, so the stored identities are never consulted there.
        #[cfg(not(unix))]
        let (chain_identity, events_identity) = ((0, 0), (0, 0));
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
        let (events, next_seq, tail_chain_mac, line_macs) =
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
        let anchor_path = events_path.with_file_name("events.anchor");
        if next_seq == 0 {
            if checkpoint_path.symlink_metadata().is_ok() {
                return Err(ardur_governance::GovernanceError::Io(
                    "evidence journal is empty but an authenticated tail checkpoint exists; \
                     the journal was truncated or deleted after events were recorded"
                        .to_string(),
                ));
            }
        } else {
            // The write order (journal → checkpoint → anchor) leaves one
            // legitimate stale state: a crash after the journal append fsync
            // but before the checkpoint publish. That journal line is fully
            // authenticated (its chain MAC verifies), so rejecting the open
            // would force manual repair of a state that is provably the
            // write-order window. Accept the checkpoint exactly one append
            // behind — only when the anchor AGREES with it (both at the
            // pre-append tail), which a rolled-back-truncation attack cannot
            // arrange without the key — then durably advance both siblings
            // before anything else runs. A checkpoint AHEAD of the journal
            // or more than one behind stays fail-closed, as does a missing
            // checkpoint beyond the first append's window.
            let checkpoint_state: Option<(u64, String)> =
                match open_regular_no_follow(&checkpoint_path, false) {
                    Ok(checkpoint_file) => {
                        let checkpoint_bytes = read_all_from(&checkpoint_file).map_err(|e| {
                            ardur_governance::GovernanceError::Io(format!(
                                "events checkpoint read: {e}"
                            ))
                        })?;
                        let checkpoint_json =
                            std::str::from_utf8(&checkpoint_bytes).map_err(|e| {
                                ardur_governance::GovernanceError::Io(format!(
                                    "checkpoint utf8: {e}"
                                ))
                            })?;
                        Some(ardur_governance::verify_evidence_checkpoint(
                            &events_mac_key,
                            checkpoint_json.trim(),
                        )?)
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        return Err(ardur_governance::GovernanceError::Io(format!(
                            "evidence journal tail checkpoint is unreadable ({e}); the journal \
                             tail may have been truncated — refusing to replay"
                        )));
                    }
                };
            match checkpoint_state {
                Some((checkpoint_seq, checkpoint_tail))
                    if checkpoint_seq == next_seq - 1 && checkpoint_tail == tail_chain_mac =>
                {
                    // Current — the steady state.
                }
                Some((checkpoint_seq, ref checkpoint_tail))
                    if checkpoint_seq + 1 == next_seq - 1
                        && line_macs[checkpoint_seq as usize] == *checkpoint_tail =>
                {
                    // One append behind. The anchor must independently agree
                    // (it was not yet rewritten either), which a deletion +
                    // checkpoint-rollback cannot forge: the steady-state
                    // anchor is at the tail, not at the checkpoint.
                    let anchor_bytes = read_all_from(
                        &open_regular_no_follow(&anchor_path, false).map_err(|e| {
                            ardur_governance::GovernanceError::Io(format!(
                                "events anchor read for crash-window recovery: {e}"
                            ))
                        })?,
                    )
                    .map_err(|e| {
                        ardur_governance::GovernanceError::Io(format!("events anchor read: {e}"))
                    })?;
                    let anchor_json = std::str::from_utf8(&anchor_bytes).map_err(|e| {
                        ardur_governance::GovernanceError::Io(format!("anchor utf8: {e}"))
                    })?;
                    let anchor_tail = ardur_governance::verify_evidence_anchor(
                        &events_mac_key,
                        anchor_json.trim(),
                    )?;
                    // With the per-append precommit, the anchor in this
                    // window is the PENDING commitment of the journaled-but-
                    // not-yet-checkpointed line — the exact signature of the
                    // write-order crash. (A Committed anchor at the
                    // checkpoint's tail would mean the line was journaled
                    // without its precommit — never legitimate.)
                    let tail_seq = checkpoint_seq + 1;
                    if anchor_tail
                        != ardur_governance::EvidenceAnchorTail::Pending(
                            tail_seq,
                            line_macs[tail_seq as usize].clone(),
                        )
                    {
                        return Err(ardur_governance::GovernanceError::Io(
                            "the checkpoint is one append behind but the anchor does not \
                             pre-commit the journaled line (not the write-order crash \
                             window); refusing to replay"
                                .to_string(),
                        ));
                    }
                    // Durably advance both siblings to the journal tail
                    // before the sweep: the recovered state must not persist.
                    write_events_checkpoint(
                        &events_path,
                        &events_mac_key,
                        next_seq - 1,
                        &tail_chain_mac,
                    )?;
                    write_events_anchor(
                        &events_path,
                        &events_mac_key,
                        ardur_governance::EvidenceAnchorTail::Committed(
                            next_seq - 1,
                            tail_chain_mac.clone(),
                        ),
                    )?;
                }
                Some(_) => {
                    return Err(ardur_governance::GovernanceError::Io(
                        "evidence journal tail does not match its authenticated checkpoint \
                         (tail truncation); refusing to replay"
                            .to_string(),
                    ));
                }
                None => {
                    // The first append's crash window: one journaled line, no
                    // checkpoint yet. It is only genuine with the
                    // PRE-COMMITTED pending anchor (published before the
                    // journal write) — read and verify it BEFORE writing
                    // anything. An editor who truncates an unmirrored pair to
                    // its first line and deletes both siblings cannot
                    // reproduce the pending commitment over the surviving
                    // line's chain MAC: the anchor's absence, a null tail, or
                    // a wrong pending MAC each prove this is NOT the window.
                    if next_seq != 1 {
                        return Err(ardur_governance::GovernanceError::Io(
                            "evidence journal tail checkpoint is missing over a multi-line \
                             journal (truncation); refusing to replay"
                                .to_string(),
                        ));
                    }
                    let existing_anchor = open_regular_no_follow(&anchor_path, false)
                        .map_err(|e| {
                            ardur_governance::GovernanceError::Io(format!(
                                "evidence anchor is unreadable ({e}) over a checkpoint-less \
                                 first line; the anchor proves the first-append crash window — \
                                 refusing to replay"
                            ))
                        })
                        .and_then(|f| {
                            read_all_from(&f).map_err(|e| {
                                ardur_governance::GovernanceError::Io(format!(
                                    "events anchor read: {e}"
                                ))
                            })
                        })?;
                    let anchor_json = std::str::from_utf8(&existing_anchor).map_err(|e| {
                        ardur_governance::GovernanceError::Io(format!("anchor utf8: {e}"))
                    })?;
                    let anchor_tail = ardur_governance::verify_evidence_anchor(
                        &events_mac_key,
                        anchor_json.trim(),
                    )?;
                    if anchor_tail
                        != ardur_governance::EvidenceAnchorTail::Pending(0, line_macs[0].clone())
                    {
                        return Err(ardur_governance::GovernanceError::Io(
                            "the anchor does not pre-commit the surviving first line (not the \
                             first-append crash window); refusing to replay"
                                .to_string(),
                        ));
                    }
                    write_events_checkpoint(&events_path, &events_mac_key, 0, &tail_chain_mac)?;
                    write_events_anchor(
                        &events_path,
                        &events_mac_key,
                        ardur_governance::EvidenceAnchorTail::Committed(0, tail_chain_mac.clone()),
                    )?;
                }
            }
        }

        // ---- Anchor: the third artifact that outlives the pair. ----
        // The checkpoint proves the journal still ends where it last
        // committed, but both files live in one deletable pair: an editor in
        // the crash window (journal records fsynced, the event ER not yet
        // chained) could delete both and the open would recreate an empty
        // journal with no contradiction. The anchor is rewritten on every
        // append and holds the committed tail under its own MAC: a missing
        // anchor over committed state, or an anchor whose tail is AHEAD of
        // the journal (or whose MAC is not the journal line's at that seq),
        // is a wholesale deletion or rollback — fail closed. An anchor one
        // transaction BEHIND the checkpoint is the benign mid-transaction
        // crash (write order journal → checkpoint → anchor) and is allowed;
        // the checkpoint remains the binding tail check. The residual limit,
        // honestly: an editor who deletes or rolls back all three evidence
        // artifacts AND no event ER has ever chained is indistinguishable
        // from first initialization — nothing signed exists yet to anchor
        // against; once any event ER chains, the reconciliation closes that
        // window.
        let anchor_bytes = match open_regular_no_follow(&anchor_path, false) {
            Ok(file) => Some(read_all_from(&file).map_err(|e| {
                ardur_governance::GovernanceError::Io(format!("events anchor read: {e}"))
            })?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(ardur_governance::GovernanceError::Io(format!(
                    "events anchor open: {e}"
                )));
            }
        };
        match anchor_bytes {
            None => {
                if next_seq > 0 {
                    return Err(ardur_governance::GovernanceError::Io(
                        "evidence anchor is missing but the journal has committed lines;                          the anchor was deleted — refusing to replay"
                            .to_string(),
                    ));
                }
                // First initialization: create the anchor (empty tail) before
                // any append, so a later wholesale deletion of the journal
                // pair is distinguishable from a store that never saw one.
                write_events_anchor(
                    &events_path,
                    &events_mac_key,
                    ardur_governance::EvidenceAnchorTail::Null,
                )?;
            }
            Some(bytes) => {
                let json = std::str::from_utf8(&bytes).map_err(|e| {
                    ardur_governance::GovernanceError::Io(format!("anchor utf8: {e}"))
                })?;
                let anchor_tail =
                    ardur_governance::verify_evidence_anchor(&events_mac_key, json.trim())?;
                // The write order (journal → checkpoint → anchor) bounds a
                // benign crash to an anchor that is current or EXACTLY one
                // transaction behind — anything older is a restored
                // snapshot, and a null anchor over a multi-line journal is
                // the initial snapshot restored past the first append. The
                // only legitimate null anchors are the empty journal and
                // the first append's crash window (next_seq == 1).
                // The legitimate states, by construction (write order:
                // pending-anchor → journal → checkpoint → committed-anchor):
                //
                // - Null: only ever a store that has not begun its first
                //   append — the journal must be empty.
                // - Pending(0, m): the first append's pre-commit survived but
                //   the committed anchor did not. Reaching this arm means the
                //   checkpoint block already validated the journal (a missing
                //   checkpoint recovered itself against this same pending
                //   state, so the anchor file now reads Committed — Pending
                //   here implies a present, current checkpoint with the crash
                //   between the checkpoint and committed-anchor publishes).
                //   Pending over an EMPTY journal is the erased-or-never-
                //   landed ambiguity: fail closed.
                // - Committed(s, m): the steady state, bounded lag
                //   {current, one behind}.
                match anchor_tail {
                    ardur_governance::EvidenceAnchorTail::Null => {
                        if next_seq != 0 {
                            return Err(ardur_governance::GovernanceError::Io(
                                "the evidence anchor is the initial null-tail snapshot but the \
                                 journal has committed lines (restored anchor); refusing to \
                                 replay"
                                    .to_string(),
                            ));
                        }
                    }
                    ardur_governance::EvidenceAnchorTail::Pending(k, ref pending_mac) => {
                        let k = k as usize;
                        let current = next_seq as usize;
                        if k >= current {
                            // The precommitted line is absent from the
                            // journal: it never landed (crash between the
                            // precommit and the append) or was deleted —
                            // indistinguishable, so fail closed.
                            return Err(ardur_governance::GovernanceError::Io(
                                "an authenticated pending append is missing from the journal \
                                 (the line never landed, or was deleted — indistinguishable); \
                                 refusing to replay"
                                    .to_string(),
                            ));
                        }
                        if line_macs[k] != *pending_mac {
                            return Err(ardur_governance::GovernanceError::Io(
                                "the pending anchor does not match the journaled line it \
                                 pre-commits; refusing to replay"
                                    .to_string(),
                            ));
                        }
                        if k + 1 != current {
                            // The write order makes a pending anchor over
                            // anything but the tail line impossible: a later
                            // append would have overwritten it.
                            return Err(ardur_governance::GovernanceError::Io(
                                "the pending anchor pre-commits a non-tail line (impossible \
                                 in the legitimate write order); refusing to replay"
                                    .to_string(),
                            ));
                        }
                        // Crash between the checkpoint and committed-anchor
                        // publishes: advance now. (The one-behind-checkpoint
                        // case recovered itself against this pending state in
                        // the checkpoint block above and rewrote the anchor
                        // to Committed, so it does not reach here.)
                        write_events_anchor(
                            &events_path,
                            &events_mac_key,
                            ardur_governance::EvidenceAnchorTail::Committed(
                                k as u64,
                                pending_mac.clone(),
                            ),
                        )?;
                    }
                    ardur_governance::EvidenceAnchorTail::Committed(anchor_seq, anchor_mac) => {
                        let idx = anchor_seq as usize;
                        let current = next_seq as usize;
                        if idx >= current || idx + 2 < current || line_macs[idx] != anchor_mac {
                            return Err(ardur_governance::GovernanceError::Io(
                                "the journal/checkpoint tail is not the anchored tail or its \
                                 immediate predecessor (wholesale deletion or rollback of the \
                                 evidence pair); refusing to replay"
                                    .to_string(),
                            ));
                        }
                        // A one-append-behind anchor is the benign
                        // checkpoint-then-crash window — but leaving it stale
                        // would let a SECOND crash plus a truncation +
                        // checkpoint rollback land in a state the stale
                        // anchor agrees with. Durably advance it to the
                        // current tail before the sweep, exactly as the
                        // checkpoint-lag recovery path does.
                        if idx + 2 == current {
                            write_events_anchor(
                                &events_path,
                                &events_mac_key,
                                ardur_governance::EvidenceAnchorTail::Committed(
                                    next_seq - 1,
                                    tail_chain_mac.clone(),
                                ),
                            )?;
                        }
                    }
                }
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
            journaled: events
                .iter()
                .map(|(pre, post)| (pre.event_id.clone(), (pre.clone(), post.clone())))
                .collect(),
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
            // Re-project with the receipt's SIGNED verifier identity, not the
            // opener's: the same governed data dir is legitimately reopened
            // under different verifier ids (the CLI and server surfaces use
            // different ones), and re-signing history under the new identity
            // would report a tampered journal over valid evidence. The
            // current verifier id is for previously unmirrored events (the
            // sweep below), not for already-signed receipts.
            let projected = project_event_execution_receipt(
                pre,
                post.as_ref(),
                er.receipt().verifier_id.as_str(),
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
            // Recovery appends are chain transactions too: verify journal
            // ownership before each (the live paths do the same).
            verify_lock_inode_for(&events_lock, &lock_path)?;
            append_chain_line(&path, chain_identity, &mut state, signed)?;
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
            events_lock,
            events_lock_path: lock_path,
            chain_identity,
            events_identity,
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

    /// The held flock covers the lock file's INODE; an editor who unlinks
    /// and recreates `events.lock` lets a second emitter lock a different
    /// inode, and both would then pass the length fork-check concurrently.
    /// Re-verify the named inode before every transaction. (A swap landing
    /// in the microseconds between this check and the append is the
    /// documented residual; the check makes the already-executed swap — the
    /// finding's scenario — impossible to survive.)
    #[cfg(unix)]
    fn verify_lock_inode(&self) -> Result<(), ardur_governance::GovernanceError> {
        verify_lock_inode_for(&self.events_lock, &self.events_lock_path)
    }

    /// Non-unix: unreachable (open fails closed without the ownership
    /// guard); kept so the crate still compiles there.
    #[cfg(not(unix))]
    fn verify_lock_inode(&self) -> Result<(), ardur_governance::GovernanceError> {
        Ok(())
    }
}

/// The held flock covers the lock file's INODE; an editor who unlinks and
/// recreates `events.lock` lets a second emitter lock a different inode, and
/// both would then pass the length fork-check concurrently. Re-verify the
/// named inode before EVERY journal or chain transaction — the journal
/// record paths, the round/event chain appends, and the open-time sweep
/// (which appends ERs before the emitter exists). (A swap landing in the
/// microseconds between this check and the append is the documented
/// residual; the check makes the already-executed swap — the finding's
/// scenario — impossible to survive.)
#[cfg(unix)]
fn verify_lock_inode_for(
    lock: &std::fs::File,
    lock_path: &Path,
) -> Result<(), ardur_governance::GovernanceError> {
    use std::os::unix::fs::MetadataExt as _;
    let held = lock
        .metadata()
        .map_err(|e| ardur_governance::GovernanceError::Io(format!("events lock stat: {e}")))?;
    let named = std::fs::symlink_metadata(lock_path).map_err(|e| {
        ardur_governance::GovernanceError::Io(format!(
            "events lock path stat: {e} (the lock file was replaced?)"
        ))
    })?;
    if held.dev() != named.dev() || held.ino() != named.ino() {
        return Err(ardur_governance::GovernanceError::Io(
            "the events.lock inode changed under us (unlinked and recreated); journal \
             ownership is no longer exclusive"
                .to_string(),
        ));
    }
    Ok(())
}

/// The opened-descriptor identity check for every path-based append: the
/// fresh descriptor must name the same inode we opened at startup (a
/// directory writer replacing the file — e.g. a hardlink to the OTHER log —
/// changes it). The opposite-file case is subsumed: a hardlink to the chain
/// has the chain's inode, not the journal's stored one.
#[cfg(unix)]
fn verify_file_identity(
    file: &std::fs::File,
    expected: (u64, u64),
    what: &str,
) -> Result<(), ardur_governance::GovernanceError> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = file
        .metadata()
        .map_err(|e| ardur_governance::GovernanceError::Io(format!("{what} identity stat: {e}")))?;
    if (meta.dev(), meta.ino()) != expected {
        return Err(ardur_governance::GovernanceError::Io(format!(
            "the {what} file was replaced since open (identity changed); refusing to append"
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_file_identity(
    _file: &std::fs::File,
    _expected: (u64, u64),
    _what: &str,
) -> Result<(), ardur_governance::GovernanceError> {
    Ok(())
}

#[cfg(not(unix))]
fn verify_lock_inode_for(
    _lock: &std::fs::File,
    _lock_path: &Path,
) -> Result<(), ardur_governance::GovernanceError> {
    Ok(())
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
        // A chain append is a transaction too: confirm the lock inode still
        // names the file we hold, or a second emitter could be racing it.
        if let Err(e) = self.verify_lock_inode() {
            state.poisoned = Some(e.to_string());
            return Err(e);
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
        append_chain_line(&self.path, self.chain_identity, &mut state, signed)
    }

    fn record_pre_effect(
        &self,
        record: &PreEffectRecord,
    ) -> Result<(), ardur_governance::GovernanceError> {
        let mut state = self.inner.lock();
        if let Some(reason) = state.poisoned.as_deref() {
            return Err(poisoned_error(reason));
        }
        if let Err(e) = self.verify_lock_inode() {
            state.poisoned = Some(e.to_string());
            return Err(e);
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
        // Precommit EVERY append in the anchor BEFORE the journal write: a
        // crash after the line fsyncs but before the checkpoint publishes
        // leaves the checkpoint and anchor exactly matching the journal
        // WITHOUT the new line, so deleting that line afterwards would read
        // as a clean tail — the pending commitment over the line's chain MAC
        // makes the deletion fail closed at the next open. (For the first
        // append it also makes the window distinguishable from pristine
        // initialization.)
        if let Err(e) = write_events_anchor(
            &self.events_path,
            &self.events_mac_key,
            ardur_governance::EvidenceAnchorTail::Pending(state.next_seq, chain_mac.clone()),
        ) {
            state.poisoned = Some(e.to_string());
            return Err(e);
        }
        append_events_line(&self.events_path, self.events_identity, &mut state, &line)?;
        if let Err(e) = write_events_checkpoint(
            &self.events_path,
            &self.events_mac_key,
            state.next_seq,
            &chain_mac,
        )
        .and_then(|()| {
            write_events_anchor(
                &self.events_path,
                &self.events_mac_key,
                ardur_governance::EvidenceAnchorTail::Committed(state.next_seq, chain_mac.clone()),
            )
        }) {
            state.poisoned = Some(e.to_string());
            return Err(e);
        }
        state.tail_chain_mac = chain_mac;
        state.next_seq += 1;
        state
            .journaled
            .insert(record.event_id.clone(), (record.clone(), None));
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
        if let Err(e) = self.verify_lock_inode() {
            state.poisoned = Some(e.to_string());
            return Err(e);
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
        // The same per-append precommit as record_pre_effect (see there).
        if let Err(e) = write_events_anchor(
            &self.events_path,
            &self.events_mac_key,
            ardur_governance::EvidenceAnchorTail::Pending(state.next_seq, chain_mac.clone()),
        ) {
            state.poisoned = Some(e.to_string());
            return Err(e);
        }
        append_events_line(&self.events_path, self.events_identity, &mut state, &line)?;
        if let Err(e) = write_events_checkpoint(
            &self.events_path,
            &self.events_mac_key,
            state.next_seq,
            &chain_mac,
        )
        .and_then(|()| {
            write_events_anchor(
                &self.events_path,
                &self.events_mac_key,
                ardur_governance::EvidenceAnchorTail::Committed(state.next_seq, chain_mac.clone()),
            )
        }) {
            state.poisoned = Some(e.to_string());
            return Err(e);
        }
        state.tail_chain_mac = chain_mac;
        state.next_seq += 1;
        if let Some(entry) = state.journaled.get_mut(&record.event_id) {
            entry.1 = Some(record.clone());
        }
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
        // A chain append is a transaction too: confirm the lock inode still
        // names the file we hold, or a second emitter could be racing it.
        if let Err(e) = self.verify_lock_inode() {
            state.poisoned = Some(e.to_string());
            return Err(e);
        }
        // Idempotent per event: a replayed terminal (a live mirror racing a
        // boot sweep, or a caller retry) must not duplicate the ER.
        if state.chained_step_ids.contains(&pre.event_id) {
            return Ok(());
        }
        // The ER must claim only what is durable: the caller-supplied records
        // must be EXACTLY the journaled ones. A cloned pre with the same
        // event id but changed actor/arguments would otherwise be signed
        // over evidence that was never persisted — reconciliation would only
        // notice at the next restart, after consumers may have accepted it.
        let journaled = state.journaled.get(&pre.event_id).cloned();
        match journaled {
            Some((journaled_pre, Some(journaled_post)))
                if journaled_pre == *pre && journaled_post == *post => {}
            _ => {
                let err = ardur_governance::GovernanceError::Io(format!(
                    "event {} was not journaled with exactly these records; refusing to \
                     sign over evidence that was never durable",
                    pre.event_id
                ));
                state.poisoned = Some(err.to_string());
                return Err(err);
            }
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
        append_chain_line(&self.path, self.chain_identity, &mut state, signed)
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
    expected_identity: (u64, u64),
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
    if let Err(e) = verify_file_identity(&file, expected_identity, "chain") {
        state.poisoned = Some(e.to_string());
        return Err(e);
    }
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

/// Rewrite one small sibling of the journal (checkpoint or anchor) with the
/// full hardened discipline: the parent directory is opened ONCE with
/// NOFOLLOW, the child is created/truncated through that anchored descriptor
/// (a directory swapped for a symlink between the journal append and this
/// open is never followed), the file is fsynced, and the directory entry is
/// fsynced through the SAME anchored descriptor — a separate path-based open
/// of the parent would follow a swap that happened in between.
#[cfg(unix)]
fn write_durable_sibling(
    events_path: &Path,
    name: &str,
    contents: &str,
) -> Result<(), ardur_governance::GovernanceError> {
    use std::io::Write as _;
    let write_result: std::io::Result<()> = (|| {
        use rustix::fs::{Mode, OFlags, openat};
        let parent_path = events_path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "journal has no parent")
        })?;
        let parent = openat(
            rustix::fs::CWD,
            parent_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
        // Atomic publish: write and fsync a temporary sibling, then rename
        // it onto the target through the anchored parent. A crash between
        // O_TRUNC and the write could otherwise leave the previous (valid)
        // document destroyed and the new one absent — with rename, the
        // previous document remains visible until its durable replacement
        // exists. The temporary is created EXCLUSIVELY: NOFOLLOW rejects
        // symlinks but not hardlinks, and a pre-planted `events.tail.tmp`
        // hardlink to the journal would otherwise be truncated in place,
        // destroying the record whose publish is about to be reported as
        // durable. A pre-existing tmp is crash residue or attack residue —
        // the flock holder owns the directory, so it is unlinked and the
        // create retried once; a second failure is an active race and fails
        // closed.
        let tmp_name = format!("{name}.tmp");
        let tmp_flags =
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut file = match openat(
            &parent,
            tmp_name.as_str(),
            tmp_flags,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(fd) => std::fs::File::from(fd),
            Err(first) => {
                rustix::fs::unlinkat(&parent, tmp_name.as_str(), rustix::fs::AtFlags::empty())
                    .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
                openat(
                    &parent,
                    tmp_name.as_str(),
                    tmp_flags,
                    Mode::from_raw_mode(0o600),
                )
                .map(std::fs::File::from)
                .map_err(|_| std::io::Error::from_raw_os_error(first.raw_os_error()))?
            }
        };
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "evidence tmp sibling is not a regular file",
            ));
        }
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        rustix::fs::renameat(&parent, tmp_name.as_str(), &parent, name)
            .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
        std::fs::File::from(parent).sync_all()
    })();
    write_result.map_err(|e| {
        ardur_governance::GovernanceError::Io(format!("evidence sibling {name} write: {e}"))
    })
}

/// Rewrite one small sibling of the journal (non-unix fallback).
#[cfg(not(unix))]
fn write_durable_sibling(
    events_path: &Path,
    name: &str,
    contents: &str,
) -> Result<(), ardur_governance::GovernanceError> {
    use std::io::Write as _;
    (|| {
        let tmp = events_path.with_file_name(format!("{name}.tmp"));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => file,
            Err(first) => {
                // Stale or planted residue: the holder owns the directory.
                std::fs::remove_file(&tmp)?;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)
                    .map_err(|_| first)?
            }
        };
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, events_path.with_file_name(name))?;
        if let Some(parent) = events_path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })()
    .map_err(|e: std::io::Error| {
        ardur_governance::GovernanceError::Io(format!("evidence sibling {name} write: {e}"))
    })
}

/// Rewrite the authenticated tail checkpoint beside the journal. The
/// checkpoint is what makes a TAIL truncation as detectable as a mid-journal
/// deletion: the chain MACs alone prove linkage, and this proves the journal
/// still ends where the emitter last committed it. Failure poisons the
/// caller the same way an append failure does (handled at the call site).
fn write_events_checkpoint(
    events_path: &Path,
    key: &[u8; 32],
    seq: u64,
    tail_chain_mac: &str,
) -> Result<(), ardur_governance::GovernanceError> {
    let json = ardur_governance::evidence_checkpoint_json(key, seq, tail_chain_mac);
    write_durable_sibling(events_path, "events.tail", &json)
}

/// Rewrite the authenticated tail anchor — the third artifact that survives
/// deletion of the journal+checkpoint pair, so a wholesale deletion in the
/// crash window (journal records fsynced, event ER not yet chained) is
/// distinguishable from first initialization at the next open.
fn write_events_anchor(
    events_path: &Path,
    key: &[u8; 32],
    tail: ardur_governance::EvidenceAnchorTail,
) -> Result<(), ardur_governance::GovernanceError> {
    let json = ardur_governance::evidence_anchor_json(key, tail);
    write_durable_sibling(events_path, "events.anchor", &json)
}

/// Append one evidence record line to the journal with the same fork guard
/// and fsync discipline as the chain. An ambiguous write poisons the emitter:
/// a possibly-missing record would silently degrade crash recovery to
/// absence otherwise, which the gap-observability obligation forbids.
fn append_events_line(
    path: &Path,
    expected_identity: (u64, u64),
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
    if let Err(e) = verify_file_identity(&file, expected_identity, "events") {
        state.poisoned = Some(e.to_string());
        return Err(e);
    }
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

/// The parsed journal: the paired records, the next sequence number, the
/// verified chain MAC of the last line (empty when the journal is empty),
/// and every line's verified chain MAC (the anchor cross-check indexes it).
type ParsedEvidenceJournal = (
    Vec<(PreEffectRecord, Option<PostEffectRecord>)>,
    u64,
    String,
    Vec<String>,
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
        return Ok((Vec::new(), 0, String::new(), Vec::new()));
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
    let mut line_macs: Vec<String> = Vec::new();
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
        line_macs.push(verified_mac.clone());
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
    let next_seq = line_macs.len() as u64;
    Ok((
        pres.into_iter()
            .map(|pre| {
                let post = posts.get(&pre.event_id).cloned();
                (pre, post)
            })
            .collect(),
        next_seq,
        prev_chain_mac,
        line_macs,
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
