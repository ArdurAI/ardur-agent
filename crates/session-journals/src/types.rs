//! The value types a journal speaks in: the per-session [`EntryId`] cursor, the
//! hex [`Sha256Digest`] a tool call records, the [`ReservationId`] a cost
//! settlement names, and the [`JournalEntry`] log record itself.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::JournalError;
use ardur_cost_gate::{CostDelta, CostTuple, UnixTsMillis};
use ardur_runtime::ReceiptId;
use ardur_tool_registry::ToolId;

/// The monotonic, per-session position of an entry in its journal.
///
/// Assigned by the journal at [`append`](crate::SessionJournal::append) time —
/// the 0-based index of the entry in the session's append-only log — and
/// returned so a caller can later resume from it. Entry ids are positional, so
/// the same sequence of appends yields the same ids on replay, and a higher id
/// always means a later entry. [`replay_from`](crate::SessionJournal::replay_from)
/// uses an `EntryId` as an exclusive cursor: it returns the entries *after* it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(pub u64);

impl EntryId {
    /// Construct an [`EntryId`] from its raw position.
    #[must_use]
    pub fn new(position: u64) -> Self {
        Self(position)
    }

    /// The raw 0-based position.
    #[must_use]
    pub fn value(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A hex-encoded SHA-256 content digest: exactly 64 lowercase hex characters
/// (the 32 raw digest bytes, two hex chars each).
///
/// A [`JournalEntry::ToolInvocation`] records the digests of the tool's input
/// and output payloads rather than the payloads themselves, so the journal
/// stays bounded while still binding each call to the exact bytes it ran on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    /// Hash `data` with SHA-256 and hex-encode the digest.
    #[must_use]
    pub fn of(data: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(data);
        Self(to_hex(&hasher.finalize()))
    }

    /// Wrap a pre-computed hex digest, validating that it is exactly 64
    /// lowercase hex characters.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::Malformed`] if `hex` is not 64 characters or
    /// contains a non-`[0-9a-f]` character.
    pub fn from_hex(hex: impl Into<String>) -> Result<Self, JournalError> {
        let hex = hex.into();
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(JournalError::Malformed(format!(
                "expected 64 lowercase hex chars, got {:?}",
                hex
            )));
        }
        Ok(Self(hex))
    }

    /// The digest as a 64-character hex string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lowercase-hex-encode a byte slice.
fn to_hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String is infallible.
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Identifier of the cost reservation a [`JournalEntry::CostFinalized`] settles.
///
/// §11.14's cost-admission gate currently mints reservations under a bare
/// `Uuid`; this newtype gives the journal a typed handle for the same value so
/// a settlement entry names which hold it released. When the cost layer grows a
/// shared `ReservationId` type this placeholder is what it replaces (see the
/// `// TODO §7.10 Phase 2:` reconciliation marker in `lib.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReservationId(pub Uuid);

impl ReservationId {
    /// Mint a fresh reservation id (UUIDv4).
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap an existing reservation `Uuid` minted by the cost-admission gate.
    #[must_use]
    pub fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }
}

impl Default for ReservationId {
    fn default() -> Self {
        Self::new()
    }
}

/// One record in a session's append-only journal.
///
/// Serde-tagged on a `"kind"` discriminant so each line of a JSONL journal is a
/// self-describing JSON object. The [`EntryId`] of an entry is *not* part of the
/// record — it is the entry's position in the log, assigned at append time —
/// which is why [`JournalEntry::Invalidation`] carries the `EntryId` it retracts
/// explicitly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum JournalEntry {
    /// A message the user sent.
    UserMessage {
        /// The message text.
        content: String,
        /// When it was recorded.
        at: UnixTsMillis,
    },

    /// A message the assistant produced, bound to the receipt that attests it.
    AssistantMessage {
        /// The message text.
        content: String,
        /// When it was recorded.
        at: UnixTsMillis,
        /// The §11.14 receipt this turn folds into.
        receipt_id: ReceiptId,
    },

    /// A tool call: the tool, the digests of its input and output payloads, and
    /// the receipt that attests it.
    ToolInvocation {
        /// The tool that ran.
        tool_id: ToolId,
        /// SHA-256 of the tool's input payload.
        input_digest: Sha256Digest,
        /// SHA-256 of the tool's output payload.
        output_digest: Sha256Digest,
        /// When it was recorded.
        at: UnixTsMillis,
        /// The §11.14 receipt this call folds into.
        receipt_id: ReceiptId,
    },

    /// A cost reservation settled: the actual billed cost and the delta
    /// refunded against the original hold.
    CostFinalized {
        /// The reservation that was settled.
        reservation_id: ReservationId,
        /// The actual cost billed.
        actual: CostTuple,
        /// The amount released back from the original hold.
        refunded: CostDelta,
        /// When it was recorded.
        at: UnixTsMillis,
    },

    /// A resume point: replay can start *after* this entry without re-reading
    /// the history before it.
    Checkpoint {
        /// Stable id of this checkpoint.
        checkpoint_id: Uuid,
        /// A human-readable summary of the state at this point.
        summary: String,
        /// When it was recorded.
        at: UnixTsMillis,
    },

    /// A retraction: an earlier entry is superseded and should be ignored on
    /// reconstruction.
    Invalidation {
        /// The entry being retracted.
        target_entry_id: EntryId,
        /// Why it was retracted.
        reason: String,
        /// When it was recorded.
        at: UnixTsMillis,
    },
}
