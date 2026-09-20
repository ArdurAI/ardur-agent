//! The opt-in ER mirror emitter (#502 Seam B7, Phase 1).
//!
//! [`ErMirrorEmitter`] is the runtime's default [`GovernanceEmitter`]: it
//! projects each **committed** round's verified admission facts into an MCEP
//! Execution Receipt, signs it with the same P-256 custody as the native
//! receipt chain (one key, one JWKS), and appends it to a hash-chained mirror
//! log — `governance/er-chain.jsonl` under the operator's data dir — using the
//! same hardened no-follow, fsync-ing append the native receipt log uses.
//!
//! Phase 1 boundaries, deliberately:
//!
//! - **One ER per committed native receipt.** The ER mirrors the round's chat
//!   admission (`chat.submit`); the tool calls the round recorded ride inside
//!   the invocation arguments as the **digests the native receipt already
//!   computed** — the mirror never sees or re-derives raw arguments/outputs.
//! - **Abandoned / cancelled turns mint no ER.** The seam is only reached
//!   after the native receipt is durable, which every cancellation path
//!   precedes; the terminal cancellation marker the native chain appends for a
//!   mid-loop cancel is deliberately not mirrored.
//! - **A mirror gap is honest absence.** A failed mirror append is logged and
//!   the turn still succeeds: the native receipt chain remains the source of
//!   truth, and the missing ER reads downstream as `insufficient_evidence`,
//!   never as compliance.
//! - **No per-tool effect classification.** Mapping each recorded tool call to
//!   its own effect-class ER requires the durable per-event evidence contract
//!   (#543) and stays out of Phase 1. The round-level
//!   [`SideEffectClass`] is the conservative statement the native receipt
//!   already supports: `None` for a pure completion round, `InternalWrite`
//!   once the round folded tool results into the durable session transcript.

use std::path::{Path, PathBuf};

use ardur_governance::{
    ActionClass, AuthOutcome, ErRoundFacts, ErSigner, ErSigningKey, EvidenceLevel,
    GovernanceEmitter, SideEffectClass, SignedExecutionReceipt, StepContext, ToolInvocation,
    project_execution_receipt, verify_er_log_lines,
};
use ardur_receipt::Es256SigningKey;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::receipts::{open_append_no_follow, open_regular_no_follow};

/// The ER `resource_family` for a mirrored chat round.
const CHAT_RESOURCE_FAMILY: &str = "llm_completion";
/// ER lifetime (`exp = iat + ttl`), matching the DESIGN.md CR-5 default.
const ER_TTL_SECS: u64 = 300;

/// A file-backed [`GovernanceEmitter`] mirroring committed rounds into a
/// hash-chained ER log signed with the runtime's own receipt custody.
///
/// [`open`](Self::open) verifies any existing mirror log against the key
/// before seeding the chain tail, so a restart resumes the chain instead of
/// forking it — the same boot contract the native receipt log follows. One
/// emitter owns one log; concurrent writers are excluded by the same
/// single-writer discipline as the native receipt log (bind one mirror per
/// receipt log / data dir).
pub struct ErMirrorEmitter {
    path: PathBuf,
    key: ErSigningKey,
    verifier_id: String,
    run_nonce: String,
    tail: Mutex<Option<SignedExecutionReceipt>>,
    /// The on-disk length after the last successful append. Each append first
    /// checks the log still ends exactly there: if a foreign writer (a second
    /// emitter over the same path, a log rotator) has changed the file, our
    /// cached tail is stale and appending would fork the chain — so the
    /// append fails closed instead of racing it.
    committed_len: Mutex<u64>,
    /// Set when an append's outcome is ambiguous (the line may be on disk
    /// while the tail was not advanced). Once poisoned, every later mirror
    /// fails fast: writing a fresh chained line onto a possibly-present
    /// predecessor would manufacture a broken chain out of uncertainty.
    poisoned: Mutex<Option<String>>,
}

impl ErMirrorEmitter {
    /// Open (or create) the mirror log at `path`, signing with the same
    /// P-256 custody as the native receipts (`receipt_key`) and stamping
    /// `verifier_id` on every ER.
    ///
    /// An existing log is fully verified (signatures then linkage) before the
    /// tail is seeded; a corrupt or foreign-key log fails the open rather
    /// than silently re-genesis.
    ///
    /// # Errors
    ///
    /// [`ardur_governance::GovernanceError::Key`] if the custody cannot be
    /// shared, `Verify`/`BrokenChain` if the existing log does not verify
    /// against it, or `Io` if the log cannot be read or created.
    pub fn open(
        path: &Path,
        receipt_key: &Es256SigningKey,
        verifier_id: &str,
    ) -> Result<Self, ardur_governance::GovernanceError> {
        let pem = receipt_key
            .to_pkcs8_pem()
            .map_err(|e| ardur_governance::GovernanceError::Key(format!("pem: {e}")))?;
        let key = ErSigningKey::from_pkcs8_pem(&pem)?;
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
        let committed_len = file
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
        let mut chain = verify_er_log_lines(&lines, &key.jwks())?;
        let tail = chain.pop();

        Ok(Self {
            path,
            key,
            verifier_id: verifier_id.to_string(),
            run_nonce,
            tail: Mutex::new(tail),
            committed_len: Mutex::new(committed_len),
            poisoned: Mutex::new(None),
        })
    }

    /// Open the mirror at the #502 Seam B7 convention:
    /// `<data_dir>/governance/er-chain.jsonl` (the `$ARDUR_DATA_DIR`
    /// layout). See [`open`](Self::open) for the custody and verification
    /// contract.
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
        if let Some(reason) = self.poisoned.lock().as_deref() {
            return Err(ardur_governance::GovernanceError::Io(format!(
                "mirror poisoned by an earlier failure: {reason}"
            )));
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

        let mut tail = self.tail.lock();
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
                parent: tail.as_ref(),
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
                *self.poisoned.lock() = Some(e.to_string());
                return Err(e);
            }
        };
        let signed = match ErSigner::sign(receipt, &self.key) {
            Ok(signed) => signed,
            Err(e) => {
                *self.poisoned.lock() = Some(e.to_string());
                return Err(e);
            }
        };

        // Durable append with the native log's hardened writer. The write and
        // the fsync are one append attempt: a failure after the line may have
        // reached the disk is AMBIGUOUS — poison the emitter rather than
        // advance the tail, so no later line can chain onto a state we are
        // not sure of. The already-minted native receipt is unaffected.
        use std::io::Write as _;
        let mut file = open_append_no_follow(&self.path)
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("append open: {e}")))?;
        // Fork guard: the log must still end exactly where our last
        // successful append left it. A concurrent writer (a second emitter
        // over the same path) or an external truncation means the cached
        // tail is stale — appending would fork the chain, so fail closed.
        let mut expected = self.committed_len.lock();
        let actual = file
            .metadata()
            .map_err(|e| ardur_governance::GovernanceError::Io(format!("stat: {e}")))?
            .len();
        if actual != *expected {
            return Err(ardur_governance::GovernanceError::Io(format!(
                "mirror log changed under us (len {actual} != committed {}): one emitter must \
                 own one mirror log",
                *expected
            )));
        }
        let append = writeln!(file, "{}", signed.jws_compact()).and_then(|()| file.sync_all());
        match append {
            Ok(()) => {
                *expected = actual + signed.jws_compact().len() as u64 + 1;
            }
            Err(e) => {
                *self.poisoned.lock() = Some(e.to_string());
                return Err(ardur_governance::GovernanceError::Io(format!(
                    "append: {e}"
                )));
            }
        }
        *tail = Some(signed);
        Ok(())
    }
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
