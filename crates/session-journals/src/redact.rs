//! Secret-shaped-pattern redaction over free-text [`JournalEntry`] fields.
//!
//! A journal entry's `content`/`summary`/`reason` fields are free text a user
//! or assistant wrote, and may carry an accidentally-pasted secret (an API
//! key, a bearer token, a password). Any surface that displays journal
//! entries outside the originating session's own trust boundary — an
//! operator dashboard, an export, a log line — should redact those fields
//! first. `ToolInvocation` and `CostFinalized` carry only structured,
//! non-free-text fields (digests, ids, costs) and are left untouched.

use crate::types::JournalEntry;

/// The default set of secret-shaped patterns this module redacts. Matches API
/// keys (OpenAI/Anthropic/OpenRouter `sk-...`, GitHub `gh[pousr]_...`, AWS
/// `AKIA...`), PEM private key blocks, and generic `token=`/`api_key=`/
/// `password is`/`secret:`-shaped natural-language leakage.
#[must_use]
pub fn default_secret_patterns() -> Vec<regex::Regex> {
    let patterns = [
        // OpenAI / Anthropic / OpenRouter API keys, including segmented
        // prefixes such as `sk-ant-...` and `sk-or-...`.
        r"(?i)\bsk-[a-z0-9_-]{16,}",
        // Generic secret-looking tokens
        r"(?i)bearer\s+[a-z0-9_\-\.]{20,}",
        r"(?i)token[a-z0-9_\-]*[:=]\s*[a-z0-9_\-\.]{8,}",
        r"(?i)api[_\-]?key[a-z0-9_\-]*[:=]\s*[a-z0-9_\-\.]{8,}",
        // Natural-language password/secret leakage
        r"(?i)pass(?:word)?\s*(?:is|=|:)\s*\S+",
        r"(?i)secret(?:\s+is|=|:)\s*\S+",
        // AWS-style access keys
        r"AKIA[0-9A-Z]{16}",
        // Private keys / certs
        r"-----BEGIN (RSA |EC |DSA |OPENSSH )?PRIVATE KEY-----[\s\S]*?-----END",
        // GitHub tokens
        r"gh[pousr]_[A-Za-z0-9_]{36,}",
    ];
    patterns
        .iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect()
}

/// Redact every match of `patterns` in `text`, replacing each with
/// `<REDACTED>`.
#[must_use]
pub fn redact_text(text: &str, patterns: &[regex::Regex]) -> String {
    let mut out = text.to_string();
    for re in patterns {
        out = re.replace_all(&out, "<REDACTED>").to_string();
    }
    out
}

/// Redact the free-text fields (`content`/`summary`/`reason`) of every entry
/// against `patterns`, returning a redacted copy. `ToolInvocation` and
/// `CostFinalized` entries are returned unchanged — they carry no free text.
#[must_use]
pub fn redact_entries(entries: &[JournalEntry], patterns: &[regex::Regex]) -> Vec<JournalEntry> {
    let mut redacted = entries.to_vec();
    for entry in &mut redacted {
        match entry {
            JournalEntry::UserMessage { content, .. }
            | JournalEntry::AssistantMessage { content, .. } => {
                *content = redact_text(content, patterns);
            }
            JournalEntry::Checkpoint { summary, .. } => {
                *summary = redact_text(summary, patterns);
            }
            JournalEntry::Invalidation { reason, .. } => {
                *reason = redact_text(reason, patterns);
            }
            JournalEntry::ToolInvocation { .. } | JournalEntry::CostFinalized { .. } => {}
        }
    }
    redacted
}

/// [`redact_entries`] using [`default_secret_patterns`].
#[must_use]
pub fn redact_entries_default(entries: &[JournalEntry]) -> Vec<JournalEntry> {
    redact_entries(entries, &default_secret_patterns())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ReservationId, Sha256Digest};
    use crate::{CostDelta, CostTuple, EntryId, ReceiptId, ToolId, UnixTsMillis};

    fn zero_cost() -> CostTuple {
        CostTuple {
            tokens_in: 0,
            tokens_out: 0,
            cents: 0,
            wall_ms: 0,
            attention_score: 0,
        }
    }

    fn zero_delta() -> CostDelta {
        CostDelta {
            tokens_in: 0,
            tokens_out: 0,
            cents: 0,
            wall_ms: 0,
            attention_score: 0,
        }
    }

    #[test]
    fn redacts_api_key_shaped_content() {
        let patterns = default_secret_patterns();
        let redacted = redact_text("here is my key sk-abcdefghijklmnopqrstuvwxyz", &patterns);
        assert_eq!(redacted, "here is my key <REDACTED>");
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        let patterns = default_secret_patterns();
        let redacted = redact_text("what's the weather like today?", &patterns);
        assert_eq!(redacted, "what's the weather like today?");
    }

    #[test]
    fn redacts_user_and_assistant_message_content() {
        let patterns = default_secret_patterns();
        let entries = vec![
            JournalEntry::UserMessage {
                content: "my token=abcdef01234567890".to_string(),
                at: UnixTsMillis::from(1u64),
            },
            JournalEntry::AssistantMessage {
                content: "no secrets here".to_string(),
                at: UnixTsMillis::from(2u64),
                receipt_id: ReceiptId::new(),
            },
        ];
        let redacted = redact_entries(&entries, &patterns);
        let JournalEntry::UserMessage { content, .. } = &redacted[0] else {
            panic!("expected UserMessage")
        };
        assert!(content.contains("<REDACTED>"));
        let JournalEntry::AssistantMessage { content, .. } = &redacted[1] else {
            panic!("expected AssistantMessage")
        };
        assert_eq!(content, "no secrets here");
    }

    #[test]
    fn redacts_checkpoint_summary_and_invalidation_reason() {
        let patterns = default_secret_patterns();
        let entries = vec![
            JournalEntry::Checkpoint {
                checkpoint_id: uuid::Uuid::nil(),
                summary: "AKIA1234567890ABCDEF leaked in summary".to_string(),
                at: UnixTsMillis::from(3u64),
            },
            JournalEntry::Invalidation {
                target_entry_id: EntryId::new(0),
                reason: "AKIA1234567890ABCDEF leaked in reason".to_string(),
                at: UnixTsMillis::from(4u64),
            },
        ];
        let redacted = redact_entries(&entries, &patterns);
        let JournalEntry::Checkpoint { summary, .. } = &redacted[0] else {
            panic!("expected Checkpoint")
        };
        assert!(summary.contains("<REDACTED>"));
        let JournalEntry::Invalidation { reason, .. } = &redacted[1] else {
            panic!("expected Invalidation")
        };
        assert!(reason.contains("<REDACTED>"));
    }

    #[test]
    fn leaves_tool_invocation_and_cost_finalized_untouched() {
        let patterns = default_secret_patterns();
        let entries = vec![
            JournalEntry::ToolInvocation {
                tool_id: ToolId::new("shell.run"),
                input_digest: Sha256Digest::of(b"in"),
                output_digest: Sha256Digest::of(b"out"),
                at: UnixTsMillis::from(5u64),
                receipt_id: ReceiptId::new(),
            },
            JournalEntry::CostFinalized {
                reservation_id: ReservationId::new(),
                actual: zero_cost(),
                refunded: zero_delta(),
                at: UnixTsMillis::from(6u64),
            },
        ];
        let redacted = redact_entries(&entries, &patterns);
        assert_eq!(redacted.len(), 2);
        // Structural equality: neither variant has a free-text field to alter.
        assert_eq!(
            serde_json::to_string(&redacted[0]).unwrap(),
            serde_json::to_string(&entries[0]).unwrap()
        );
        assert_eq!(
            serde_json::to_string(&redacted[1]).unwrap(),
            serde_json::to_string(&entries[1]).unwrap()
        );
    }
}
