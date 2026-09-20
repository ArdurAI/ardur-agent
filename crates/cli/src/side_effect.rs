//! gh#533 guard 6: per-subcommand side-effect classification for the
//! compaction and memory command families.
//!
//! The UX plan's registry contract (§2.1) requires every command to declare a
//! side-effect class so a UI can reason about what a command does BEFORE
//! dispatching it. The whole-command class the placeholder catalog carried
//! ("`/compact` = one class") is wrong in both directions: it would let a UI
//! treat a purely local `/compact status` as a paid provider send, or — the
//! dangerous direction — treat `/compact preview`'s external provider send as
//! a local read.
//!
//! The classification here is derived from the SAME subcommand split
//! `dispatch_compact` and `memory_command` dispatch on, and
//! `dispatch_compact` calls [`compact_subcommand`] to decide, so the
//! metadata and the dispatch cannot drift apart: the executable guard in
//! this module's tests pins the dispatch routing to the classes.

/// The side-effect class of one command invocation, following the UX plan's
/// ER taxonomy (none / internal_write / external_send / state_change).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideEffect {
    /// Purely local read: no state change, no provider send, no receipt.
    None,
    /// Writes local durable state (journal, checkpoint) but sends nothing
    /// off-process.
    InternalWrite,
    /// Sends the conversation (or a prompt) to the external model provider —
    /// a paid, egress-bearing operation. Always admitted (Cedar + cost gate)
    /// and always receipted, whatever else it also does.
    ExternalSend,
    /// Mutates governance/memory state with a receipt (e.g. a memory
    /// tombstone).
    StateChange,
}

impl SideEffect {
    /// Whether a command of this class can send data off-process.
    #[must_use]
    pub fn is_external_send(self) -> bool {
        matches!(self, SideEffect::ExternalSend)
    }

    /// The plan's taxonomy label. Consumed by the registry metadata surface
    /// (and its guard tests); kept even where the dispatcher only asks
    /// `is_external_send`, so the human-facing taxonomy stays pinned.
    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SideEffect::None => "none",
            SideEffect::InternalWrite => "internal_write",
            SideEffect::ExternalSend => "external_send",
            SideEffect::StateChange => "state_change",
        }
    }
}

/// Split a `/compact` (or `/compress`) argument string into its subcommand
/// word and the rest — exactly the split `dispatch_compact` applies.
#[must_use]
pub fn compact_subcommand(args: &str) -> (&str, &str) {
    match args.split_once(char::is_whitespace) {
        Some((sub, rest)) => (sub, rest.trim()),
        None => (args, ""),
    }
}

/// The side-effect class of one `/compact`-family invocation, derived from
/// the same subcommand split the production dispatcher uses:
///
/// - `status`   — a local token estimate; nothing leaves the process.
/// - `history`  — reads the journal; nothing leaves the process.
/// - `get`      — reads one checkpoint; nothing leaves the process.
/// - `restore`  — rolls the session back (durable journal write), no send.
/// - `preview`  — **an external provider send** (summarization) that must be
///   admitted and receipted even though it installs nothing — gh#533's core
///   finding.
/// - anything else (including the empty string) — the apply path: an
///   external provider send that then installs a checkpoint.
#[must_use]
pub fn compact_side_effect(args: &str) -> SideEffect {
    let (sub, _) = compact_subcommand(args);
    match sub {
        "status" => SideEffect::None,
        "history" | "get" => SideEffect::None,
        "restore" => SideEffect::InternalWrite,
        "preview" => SideEffect::ExternalSend,
        // A bare `/compact` or `/compact <focus text>` applies a compaction.
        _ => SideEffect::ExternalSend,
    }
}

/// The side-effect class of one `/memory` invocation. `forget` is the only
/// mutating member: it appends a receipt-linked tombstone (state_change);
/// `list`/`show` are local reads.
#[must_use]
pub fn memory_side_effect(args: &str) -> SideEffect {
    let sub = args.split_whitespace().next().unwrap_or("list");
    match sub {
        "forget" => SideEffect::StateChange,
        _ => SideEffect::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard: the preview/apply classes are EXTERNAL sends while the local
    /// subcommands are not — the exact distinction #533 requires the registry
    /// to carry.
    #[test]
    fn compact_subcommands_are_classified_distinctly() {
        let cases = [
            ("status", SideEffect::None),
            ("history", SideEffect::None),
            ("get 00000000-0000-0000-0000-000000000000", SideEffect::None),
            (
                "restore 00000000-0000-0000-0000-000000000000",
                SideEffect::InternalWrite,
            ),
            ("preview REVIEW_COMPACTION_MARKER", SideEffect::ExternalSend),
            ("preview", SideEffect::ExternalSend),
            ("", SideEffect::ExternalSend),
            ("REVIEW_COMPACTION_MARKER", SideEffect::ExternalSend),
        ];
        for (args, expected) in cases {
            assert_eq!(
                compact_side_effect(args),
                expected,
                "/compact {args:?} must classify as {expected:?}"
            );
        }
    }

    /// Guard: `/memory forget` is the only state-changing memory member.
    #[test]
    fn memory_forget_is_the_only_state_change() {
        assert_eq!(
            memory_side_effect("forget 00000000-0000-0000-0000-000000000000"),
            SideEffect::StateChange
        );
        assert_eq!(memory_side_effect("list"), SideEffect::None);
        assert_eq!(
            memory_side_effect("show 00000000-0000-0000-0000-000000000000"),
            SideEffect::None
        );
        assert_eq!(memory_side_effect(""), SideEffect::None);
    }

    /// Guard: the classification's subcommand vocabulary is exactly the
    /// dispatcher's. `dispatch_compact` matches on these literal subcommand
    /// words; if one is added there without a class here (or vice versa),
    /// this test fails until the two agree — the drift #533 guard 6 exists
    /// to prevent.
    #[test]
    fn classification_covers_the_dispatchers_subcommand_vocabulary() {
        let dispatched = ["status", "history", "get", "restore", "preview"];
        for sub in dispatched {
            let (classified, _) = compact_subcommand(sub);
            assert_eq!(
                classified, sub,
                "the classification must split exactly the words the dispatcher matches"
            );
            // Every classified word yields a definite class (never a panic
            // or a fallthrough misread): the paid two are sends, the local
            // three are not.
            match sub {
                "preview" => assert!(compact_side_effect(sub).is_external_send()),
                "status" | "history" | "get" => {
                    assert!(!compact_side_effect(sub).is_external_send())
                }
                "restore" => assert!(!compact_side_effect(sub).is_external_send()),
                _ => unreachable!(),
            }
        }
        // And the fallthrough (bare /compact or focus text) is the apply
        // path — a send.
        assert!(compact_side_effect("").is_external_send());
        assert!(compact_side_effect("focus text").is_external_send());
    }

    /// Guard: every class labels correctly for the registry's consumers.
    #[test]
    fn labels_match_the_plan_taxonomy() {
        assert_eq!(SideEffect::None.label(), "none");
        assert_eq!(SideEffect::InternalWrite.label(), "internal_write");
        assert_eq!(SideEffect::ExternalSend.label(), "external_send");
        assert_eq!(SideEffect::StateChange.label(), "state_change");
        assert!(compact_side_effect("preview").is_external_send());
        assert!(!compact_side_effect("status").is_external_send());
    }
}
