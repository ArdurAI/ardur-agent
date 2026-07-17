//! Property: [`derive_for_loop_budget`] never yields a budget looser than the
//! parent, and accepts every genuine tightening.

use ardur_loop_detector::{LoopBudget, LoopBudgetRequest, derive_for_loop_budget};
use proptest::prelude::*;

proptest! {
    #[test]
    fn a_request_at_or_below_parent_always_derives_and_never_relaxes(
        parent_n in 1u32..50,
        child_n in 0u32..100,
    ) {
        let parent = LoopBudget { same_tool_same_args_count_threshold: parent_n, ..LoopBudget::default() };
        let req = LoopBudgetRequest {
            same_tool_same_args_count_threshold: Some(child_n),
            ..LoopBudgetRequest::default()
        };
        match derive_for_loop_budget(&parent, &req) {
            Ok(derived) => {
                // Success is only possible when the request tightened (or held).
                prop_assert!(child_n <= parent_n);
                prop_assert!(derived.same_tool_same_args_count_threshold <= parent_n);
                prop_assert_eq!(derived.same_tool_same_args_count_threshold, child_n);
            }
            Err(_) => {
                // Failure is only possible when the request tried to relax.
                prop_assert!(child_n > parent_n);
            }
        }
    }

    #[test]
    fn an_empty_request_inherits_the_parent_verbatim(
        n in 1u32..50,
        k in 1u32..50,
    ) {
        let parent = LoopBudget {
            same_tool_same_args_count_threshold: n,
            no_progress_turns_threshold: k,
            ..LoopBudget::default()
        };
        let derived = derive_for_loop_budget(&parent, &LoopBudgetRequest::default()).unwrap();
        prop_assert_eq!(derived.same_tool_same_args_count_threshold, n);
        prop_assert_eq!(derived.no_progress_turns_threshold, k);
    }
}
