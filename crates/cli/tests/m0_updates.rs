//! Semantic reduction has no writer, theme, terminal, or runtime side effects.

#[path = "support/m0_cases.rs"]
mod cases;

use std::cell::Cell;
use std::collections::VecDeque;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use ardur_cli::{StreamOutcome, ToolCallInfo, TurnReducer, Update, UpdateStream, Verdict};
use ardur_fused_runtime::{FusedEvent, StageKind};
use ardur_provider_runtime::{FinishReason, Usage};
use ardur_runtime::{ReceiptId, RuntimeError, ToolCall};
use futures::{Stream, StreamExt as _, stream::FusedStream};

#[test]
fn every_stage_maps_without_claiming_a_verdict_or_committing_content() {
    let stages = [
        StageKind::CapTokenVerify,
        StageKind::CedarCheck,
        StageKind::InjectionScan,
        StageKind::CostGateAdmit,
        StageKind::ProviderStream,
        StageKind::ToolExec,
        StageKind::ReceiptMint,
        StageKind::CostGateFinalize,
        StageKind::MemoryRecord,
        StageKind::JournalAppend,
    ];
    let mut reducer = TurnReducer::default();
    for stage in stages {
        assert!(
            matches!(reducer.reduce(Ok(FusedEvent::StageStart { stage })), Some(Update::StageStart { stage: actual }) if actual == stage)
        );
        for ok in [false, true] {
            assert!(
                matches!(reducer.reduce(Ok(FusedEvent::StageEnd { stage, ok })), Some(Update::StageEnd { stage: actual, ok: actual_ok }) if actual == stage && actual_ok == ok)
            );
        }
    }
    assert_eq!(reducer.outcome(), &StreamOutcome::default());
    assert!(!reducer.is_terminated());
}

#[test]
fn interleaved_tools_yield_shared_arguments_and_exact_scanned_results() {
    let mut reducer = TurnReducer::default();
    for (id, name) in [("a", "read"), ("b", "search")] {
        assert!(
            matches!(reducer.reduce(cases::start(id, name)), Some(Update::ToolCallStart { id: actual, name: actual_name }) if actual == id && actual_name == name)
        );
        assert_eq!(
            reducer.pending_tools()[id],
            ToolCallInfo {
                name: name.into(),
                arguments: String::new()
            }
        );
    }
    for (id, text) in [
        ("a", "{\"path\":"),
        ("b", "{\"query\":\"rust\"}"),
        ("a", "\"lib.rs\"}"),
    ] {
        assert!(
            matches!(reducer.reduce(cases::delta(id, text)), Some(Update::ToolCallDelta { id: actual, delta }) if actual == id && delta == text)
        );
    }
    for (id, name, arguments) in [
        ("b", "search", "{\"query\":\"rust\"}"),
        ("a", "read", "{\"path\":\"lib.rs\"}"),
    ] {
        let scanned =
            serde_json::json!({"nested": [null, {"redacted": "[REDACTED]"}], "ok": false});
        let update = reducer
            .reduce(Ok(FusedEvent::ToolCallResult {
                id: id.into(),
                result: scanned.clone(),
            }))
            .unwrap();
        let Update::ToolCallResult {
            id: actual,
            call,
            result,
        } = update
        else {
            panic!("tool result mapping")
        };
        assert_eq!(actual, id);
        assert_eq!(
            result, scanned,
            "do not re-read, parse, or replace scanned output"
        );
        assert_eq!(
            call,
            Some(ToolCallInfo {
                name: name.into(),
                arguments: arguments.into()
            })
        );
        assert!(!reducer.pending_tools().contains_key(id));
    }
    assert!(reducer.pending_tools().is_empty());
    assert_eq!(reducer.outcome().tool_calls, ["read", "search"]);
}

#[test]
fn unknown_tool_ids_and_duplicate_starts_keep_legacy_assembly_rules() {
    let mut reducer = TurnReducer::default();
    assert!(
        matches!(reducer.reduce(cases::delta("missing", "fragment")), Some(Update::ToolCallDelta { id, delta }) if id == "missing" && delta == "fragment")
    );
    assert!(reducer.pending_tools().is_empty());
    assert!(
        matches!(reducer.reduce(cases::result("missing")), Some(Update::ToolCallResult { id, call: None, .. }) if id == "missing")
    );
    reducer.reduce(cases::start("a", "old"));
    reducer.reduce(cases::delta("a", "discarded"));
    reducer.reduce(cases::start("a", "new"));
    reducer.reduce(cases::delta("a", "not JSON\n"));
    assert_eq!(reducer.pending_tools()["a"].arguments, "not JSON\n");
    assert!(
        matches!(reducer.reduce(cases::result("a")), Some(Update::ToolCallResult { call: Some(ToolCallInfo { name, arguments }), .. }) if name == "new" && arguments == "not JSON\n")
    );
    assert!(matches!(
        reducer.reduce(cases::result("a")),
        Some(Update::ToolCallResult { call: None, .. })
    ));
    assert_eq!(reducer.outcome().tool_calls, ["old", "new"]);
}

#[test]
fn usage_saturates_but_never_substitutes_for_receipt_cost() {
    let mut reducer = TurnReducer::default();
    let first = Usage {
        tokens_in: u32::MAX,
        tokens_out: u32::MAX,
        cost_cents: Some(u64::MAX),
    };
    assert!(
        matches!(reducer.reduce(Ok(FusedEvent::Usage(first))), Some(Update::Usage { usage, total }) if usage == first && total == first)
    );
    assert!(
        matches!(reducer.reduce(cases::usage(1, 2, Some(3))), Some(Update::Usage { total, .. }) if total == first)
    );
    assert_eq!(reducer.outcome().cost_cents, None);
    for costs in [[Some(9), None, Some(7)], [None, Some(9), Some(7)]] {
        let mut reducer = TurnReducer::default();
        for cost in costs {
            reducer.reduce(cases::usage(1, 1, cost));
        }
        assert_eq!(
            reducer.outcome().usage,
            Some(Usage {
                tokens_in: 3,
                tokens_out: 3,
                cost_cents: None
            })
        );
        assert_eq!(reducer.outcome().cost_cents, None);
    }
}

#[test]
fn receipts_commit_rounds_immediately_with_authoritative_saturating_cost() {
    let mut reducer = TurnReducer::default();
    for (id, text, cost_cents, total) in [
        (1, "first", 0, 0),
        (2, "second", u64::MAX, u64::MAX),
        (3, "", 7, u64::MAX),
    ] {
        reducer.reduce(cases::content(text));
        let receipt_id = ReceiptId(uuid::Uuid::from_u128(id));
        let hash = format!("{id:064x}");
        let update = reducer.reduce(cases::receipt(id, cost_cents)).unwrap();
        let Update::ReceiptMinted {
            receipt_id: actual_id,
            chain_hash,
            cost_cents: actual_cost,
            total_cost_cents,
            committed_content,
        } = update
        else {
            panic!("receipt mapping")
        };
        assert_eq!(actual_id, receipt_id);
        assert_eq!(chain_hash, hash);
        assert_eq!(actual_cost, cost_cents);
        assert_eq!(total_cost_cents, total);
        assert_eq!(committed_content, text);
        assert_eq!(reducer.outcome().cost_cents, Some(total));
    }
    reducer.reduce(cases::content("uncommitted"));
    assert_eq!(
        reducer.outcome().committed_assistant_messages,
        ["first", "second", ""]
    );
    assert_eq!(reducer.outcome().content, "firstseconduncommitted");
    assert_eq!(
        reducer.outcome().receipt_ids,
        [1, 2, 3].map(|id| ReceiptId(uuid::Uuid::from_u128(id)))
    );
    assert_eq!(reducer.outcome().usage, None);
}

fn errors() -> Vec<RuntimeError> {
    vec![
        RuntimeError::CapTokenMissing,
        RuntimeError::CapTokenExpired,
        RuntimeError::CapDenied {
            reason: "private verifier diagnostic".into(),
        },
        RuntimeError::PolicyDenied {
            reason: "private policy diagnostic".into(),
        },
        RuntimeError::CostCeilingExceeded,
        RuntimeError::ProviderUnavailable,
        RuntimeError::TurnCancelled,
        RuntimeError::VetoedByHook {
            hook_id: "hook".into(),
            reason: "private hook diagnostic".into(),
        },
        RuntimeError::ProvisioningFailed {
            subject: "subject".into(),
            reason: "private budget diagnostic".into(),
        },
        RuntimeError::injection_blocked("filter", "private scan diagnostic", vec![]),
        RuntimeError::UnknownTool {
            tool: "unknown".into(),
        },
        RuntimeError::ToolLoopExhausted { iterations: 3 },
        RuntimeError::ToolTimeout {
            tool: "slow".into(),
        },
        RuntimeError::StreamedContentCapExceeded {
            limit: 2,
            actual: 3,
        },
        RuntimeError::ApprovalRequired {
            approval_id: "card-1".into(),
            tool: "file.write".into(),
            reason: "private pending diagnostic".into(),
        },
        RuntimeError::ApprovalRejected {
            approval_id: "card-1".into(),
            tool: "file.write".into(),
            reason: "private rejection diagnostic".into(),
        },
        RuntimeError::CommandNotFound("command".into()),
        RuntimeError::Internal(anyhow::anyhow!("private internal diagnostic")),
    ]
}

#[test]
fn errors_preserve_every_typed_variant_and_latch_reduction() {
    for error in errors() {
        let expected_variant = std::mem::discriminant(&error);
        let expected_diagnostic = error.to_string();
        let mut reducer = TurnReducer::default();
        reducer.reduce(cases::content("partial"));
        let Update::Error(actual) = reducer.reduce(Err(error)).unwrap() else {
            panic!("typed failure update")
        };
        assert_eq!(std::mem::discriminant(&actual), expected_variant);
        assert_eq!(actual.to_string(), expected_diagnostic);
        assert_eq!(
            reducer.outcome().error.as_deref(),
            Some(expected_diagnostic.as_str())
        );
        assert!(reducer.is_terminated());
        let saved = reducer.outcome().clone();
        assert!(reducer.reduce(cases::content("ignored")).is_none());
        assert!(reducer.reduce(cases::receipt(1, 100)).is_none());
        assert_eq!(reducer.into_outcome(), saved);
    }
}

#[test]
fn approval_pending_and_rejected_keep_card_tool_and_reason_fields() {
    for error in errors() {
        let pending = matches!(error, RuntimeError::ApprovalRequired { .. });
        if !pending && !matches!(error, RuntimeError::ApprovalRejected { .. }) {
            continue;
        }
        let mut reducer = TurnReducer::default();
        let Update::Error(actual) = reducer.reduce(Err(error)).unwrap() else {
            panic!("error update")
        };
        let (approval_id, tool, reason) = match actual {
            RuntimeError::ApprovalRequired {
                approval_id,
                tool,
                reason,
            } if pending => (approval_id, tool, reason),
            RuntimeError::ApprovalRejected {
                approval_id,
                tool,
                reason,
            } if !pending => (approval_id, tool, reason),
            _ => panic!("approval distinctions must not be flattened"),
        };
        assert_eq!(approval_id, "card-1");
        assert_eq!(tool, "file.write");
        assert_eq!(
            reason,
            if pending {
                "private pending diagnostic"
            } else {
                "private rejection diagnostic"
            }
        );
    }
}

#[test]
fn finish_reasons_are_forwarded_without_stopping_or_inventing_verification() {
    for reason in [
        FinishReason::Stop,
        FinishReason::MaxTokens,
        FinishReason::StopSequence("END".into()),
        FinishReason::Error("diagnostic".into()),
        FinishReason::ToolUse(vec![ToolCall {
            id: "a".into(),
            name: "read".into(),
            arguments: serde_json::json!({"x": 1}),
        }]),
    ] {
        let mut reducer = TurnReducer::default();
        assert!(
            matches!(reducer.reduce(Ok(FusedEvent::Finish(reason.clone()))), Some(Update::Finish(actual)) if actual == reason)
        );
        assert_eq!(reducer.outcome().finish_reason.as_ref(), Some(&reason));
        assert!(!reducer.is_terminated());
        assert!(matches!(
            reducer.reduce(cases::content("after finish")),
            Some(Update::ContentDelta(_))
        ));
    }
}

#[test]
fn verdict_seam_is_three_valued_and_source_events_never_synthesize_verification() {
    assert_eq!(Verdict::default(), Verdict::InsufficientEvidence);
    for (verdict, wire) in [
        (Verdict::Compliant, "compliant"),
        (Verdict::Violation, "violation"),
        (Verdict::InsufficientEvidence, "insufficient_evidence"),
    ] {
        assert_eq!(serde_json::to_value(verdict).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<Verdict>(serde_json::json!(wire)).unwrap(),
            verdict
        );
        assert!(matches!(Update::Verdict(verdict), Update::Verdict(actual) if actual == verdict));
    }
    for invalid in ["unknown", "pending", "verified"] {
        assert!(serde_json::from_value::<Verdict>(serde_json::json!(invalid)).is_err());
    }
    for (_, events) in cases::cases() {
        let mut reducer = TurnReducer::default();
        for item in events {
            assert!(!matches!(reducer.reduce(item), Some(Update::Verdict(_))));
        }
    }
}

struct Probe {
    polls: Rc<Cell<usize>>,
    dropped: Rc<Cell<bool>>,
    steps: VecDeque<Poll<Option<cases::SourceItem>>>,
}
impl Stream for Probe {
    type Item = cases::SourceItem;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.polls.set(self.polls.get() + 1);
        self.steps.pop_front().expect("source polled beyond script")
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

#[test]
fn update_stream_is_pull_based_and_drops_its_source_without_prefetch() {
    let polls = Rc::new(Cell::new(0));
    let dropped = Rc::new(Cell::new(false));
    let source = Probe {
        polls: polls.clone(),
        dropped: dropped.clone(),
        steps: VecDeque::from([
            Poll::Pending,
            Poll::Ready(Some(cases::content("first"))),
            Poll::Ready(Some(cases::content("not pulled"))),
        ]),
    };
    let mut updates = UpdateStream::new(source);
    assert_eq!(polls.get(), 0, "construction must not poll");
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    assert!(Pin::new(&mut updates).poll_next(&mut cx).is_pending());
    assert_eq!(polls.get(), 1);
    assert_eq!(updates.reducer().outcome().content, "");
    assert!(
        matches!(Pin::new(&mut updates).poll_next(&mut cx), Poll::Ready(Some(Update::ContentDelta(text))) if text == "first")
    );
    assert_eq!(polls.get(), 2);
    assert_eq!(updates.reducer().outcome().content, "first");
    assert!(!dropped.get());
    let outcome = updates.into_outcome();
    assert_eq!(outcome.content, "first");
    assert!(dropped.get());
    assert_eq!(polls.get(), 2, "taking outcome must not drain");
}

#[test]
fn update_stream_latches_error_and_eof_without_polling_source_again() {
    for item in [Some(Err(RuntimeError::ProviderUnavailable)), None] {
        let is_error = item.is_some();
        let polls = Rc::new(Cell::new(0));
        let dropped = Rc::new(Cell::new(false));
        let source = Probe {
            polls: polls.clone(),
            dropped: dropped.clone(),
            steps: VecDeque::from([Poll::Ready(item)]),
        };
        let mut updates = UpdateStream::new(source);
        assert!(!updates.is_terminated());
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        let update = Pin::new(&mut updates).poll_next(&mut cx);
        assert!(if is_error {
            matches!(
                update,
                Poll::Ready(Some(Update::Error(RuntimeError::ProviderUnavailable)))
            )
        } else {
            matches!(update, Poll::Ready(None))
        });
        assert!(updates.is_terminated());
        for _ in 0..3 {
            assert!(matches!(
                Pin::new(&mut updates).poll_next(&mut cx),
                Poll::Ready(None)
            ));
        }
        assert_eq!(polls.get(), 1);
        assert!(!dropped.get(), "latching must not change source ownership");
        drop(updates);
        assert!(dropped.get());
    }
}

#[test]
fn pinned_non_unpin_source_works_without_tasks_or_a_runtime() {
    futures::executor::block_on(async {
        let source = futures::stream::once(async { cases::content("pinned") });
        futures::pin_mut!(source);
        let mut updates = UpdateStream::new(source);
        assert!(
            matches!(updates.next().await, Some(Update::ContentDelta(text)) if text == "pinned")
        );
        assert!(updates.next().await.is_none());
        assert_eq!(updates.into_outcome().content, "pinned");
    });
}

#[test]
fn content_deltas_reduce_without_a_renderer() {
    let mut reducer = TurnReducer::default();
    for text in ["first", "", "\nsecond"] {
        assert!(
            matches!(reducer.reduce(Ok(FusedEvent::Content(text.into()))), Some(Update::ContentDelta(delta)) if delta == text)
        );
    }
    assert_eq!(reducer.outcome().content, "first\nsecond");
    assert!(reducer.outcome().committed_assistant_messages.is_empty());
    assert_eq!(reducer.outcome().cost_cents, None);
}

#[test]
fn every_scripted_outcome_matches_the_frozen_base_without_rendering() {
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/m0/baseline.json")).unwrap();
    let mut differences = Vec::new();
    for (name, events) in cases::cases() {
        let mut reducer = TurnReducer::default();
        for item in events {
            if reducer.reduce(item).is_none() {
                break;
            }
        }
        let expected = baseline[format!("plain/{name}")]["outcome_debug"]
            .as_str()
            .unwrap();
        if format!("{:#?}", reducer.outcome()) != expected {
            differences.push(name);
        }
    }
    assert!(
        differences.is_empty(),
        "outcome drift against pre-M0 base: {differences:?}"
    );
}
