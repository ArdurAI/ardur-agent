//! Same-tool-same-args whitelist evaluation.
//!
//! Some tools are legitimately repetitive: a status poller asks "is job X done?"
//! every turn; a paginator walks a cursor. Counting those as loops would
//! false-positive. Whitelisting is **per-signal** — an exempted tool is only
//! spared the repetition signal; if it runs in a no-progress or cost-accelerating
//! loop it still trips Signals 2 and 3.

use crate::sealed::Sealed;
use crate::types::{WhitelistEntry, WhitelistKind};

/// Decide whether an admission is exempt from same-tool-same-args counting.
///
/// - [`WhitelistKind::BlanketExempt`] always exempts the tool.
/// - [`WhitelistKind::Polling`] exempts it when the admission carries a polling
///   key (the caller is re-polling the same subject, which is progress-shaped).
/// - [`WhitelistKind::Pagination`] exempts it when the admission carries a
///   pagination cursor (walking pages is forward progress).
pub fn evaluate_whitelist(
    tool_name: &str,
    has_polling_key: bool,
    has_pagination_cursor: bool,
    entries: &[WhitelistEntry],
) -> bool {
    entries.iter().any(|e| {
        e.tool_name == tool_name
            && match e.kind {
                WhitelistKind::BlanketExempt => true,
                WhitelistKind::Polling => has_polling_key,
                WhitelistKind::Pagination => has_pagination_cursor,
            }
    })
}

/// Closed evaluator surface. Single workspace impl ([`DefaultWhitelistEvaluator`]);
/// external crates cannot substitute alternative suppression logic.
pub trait WhitelistEvaluator: Sealed {
    /// Whether the admission is exempt from repetition counting.
    fn is_exempt(
        &self,
        tool_name: &str,
        has_polling_key: bool,
        has_pagination_cursor: bool,
        entries: &[WhitelistEntry],
    ) -> bool;
}

/// The default rule-based evaluator, delegating to [`evaluate_whitelist`].
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultWhitelistEvaluator;

impl Sealed for DefaultWhitelistEvaluator {}

impl WhitelistEvaluator for DefaultWhitelistEvaluator {
    fn is_exempt(
        &self,
        tool_name: &str,
        has_polling_key: bool,
        has_pagination_cursor: bool,
        entries: &[WhitelistEntry],
    ) -> bool {
        evaluate_whitelist(tool_name, has_polling_key, has_pagination_cursor, entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polling_exempt_only_with_key() {
        let entries = vec![WhitelistEntry {
            tool_name: "job_status".into(),
            kind: WhitelistKind::Polling,
        }];
        assert!(evaluate_whitelist("job_status", true, false, &entries));
        assert!(!evaluate_whitelist("job_status", false, false, &entries));
        assert!(!evaluate_whitelist("web_search", true, false, &entries));
    }

    #[test]
    fn pagination_exempt_only_with_cursor() {
        let entries = vec![WhitelistEntry {
            tool_name: "list_files".into(),
            kind: WhitelistKind::Pagination,
        }];
        assert!(evaluate_whitelist("list_files", false, true, &entries));
        assert!(!evaluate_whitelist("list_files", false, false, &entries));
    }

    #[test]
    fn blanket_always_exempt() {
        let entries = vec![WhitelistEntry {
            tool_name: "noop".into(),
            kind: WhitelistKind::BlanketExempt,
        }];
        assert!(evaluate_whitelist("noop", false, false, &entries));
    }
}
