//! Scripted source events, captured against the pre-M0 renderer, not a runtime smoke.

use ardur_fused_runtime::{FusedEvent, StageKind};
use ardur_provider_runtime::{FinishReason, Usage};
use ardur_runtime::{ReceiptId, RuntimeError};

pub type SourceItem = Result<FusedEvent, RuntimeError>;

pub fn content(text: &str) -> SourceItem {
    Ok(FusedEvent::Content(text.into()))
}

pub fn receipt(id: u128, cost_cents: u64) -> SourceItem {
    Ok(FusedEvent::Receipt {
        receipt_id: ReceiptId(uuid::Uuid::from_u128(id)),
        chain_hash: format!("{id:064x}"),
        cost_cents,
    })
}

pub fn start(id: &str, name: &str) -> SourceItem {
    Ok(FusedEvent::ToolCallStart {
        id: id.into(),
        name: name.into(),
    })
}

pub fn delta(id: &str, delta: &str) -> SourceItem {
    Ok(FusedEvent::ToolCallDelta {
        id: id.into(),
        delta: delta.into(),
    })
}

pub fn result(id: &str) -> SourceItem {
    Ok(FusedEvent::ToolCallResult {
        id: id.into(),
        result: serde_json::json!({"output": "SCANNED_RESULT_NOT_RENDERED"}),
    })
}

pub fn usage(tokens_in: u32, tokens_out: u32, cost_cents: Option<u64>) -> SourceItem {
    Ok(FusedEvent::Usage(Usage {
        tokens_in,
        tokens_out,
        cost_cents,
    }))
}

pub fn cases() -> Vec<(&'static str, Vec<SourceItem>)> {
    vec![
        ("empty_eof", vec![]),
        (
            "chunks",
            vec![
                content("Hello, "),
                content("stream!"),
                Ok(FusedEvent::Finish(FinishReason::Stop)),
            ],
        ),
        (
            "newlines",
            vec![
                content("one\n"),
                content("\n"),
                content("two\n"),
                Ok(FusedEvent::Finish(FinishReason::Stop)),
            ],
        ),
        (
            "empty_clears_newline",
            vec![
                content("one"),
                content(""),
                Ok(FusedEvent::Finish(FinishReason::MaxTokens)),
            ],
        ),
        ("empty_first", vec![content(""), content("after empty")]),
        ("partial_eof", vec![content("unfinished")]),
        (
            "empty_last_eof",
            vec![content("no final newline"), content("")],
        ),
        (
            "multi_tool",
            vec![
                content("plan"),
                start("a", "file.read"),
                start("b", "search"),
                delta("a", "{\"path\":"),
                delta("b", "{\"query\":\"rust\"}"),
                delta("a", "\"src/lib.rs\"}"),
                result("b"),
                content("\nnext"),
                result("a"),
                content("done\n"),
            ],
        ),
        (
            "unknown_tool",
            vec![
                content("before"),
                delta("absent", "ignored"),
                result("absent"),
                content("after"),
            ],
        ),
        (
            "duplicate_tool",
            vec![
                start("a", "old"),
                delta("a", "discarded"),
                start("a", "replacement"),
                delta("a", "not JSON\nsecond line"),
                result("a"),
                result("a"),
            ],
        ),
        (
            "pending_tool_eof",
            vec![start("a", "pending"), delta("a", "{")],
        ),
        ("empty_tool_args", vec![start("a", "empty"), result("a")]),
        (
            "stages",
            vec![
                Ok(FusedEvent::StageStart {
                    stage: StageKind::CedarCheck,
                }),
                Ok(FusedEvent::StageEnd {
                    stage: StageKind::CedarCheck,
                    ok: false,
                }),
                Ok(FusedEvent::StageEnd {
                    stage: StageKind::JournalAppend,
                    ok: true,
                }),
            ],
        ),
        (
            "error_before_receipt",
            vec![
                content("partial"),
                Err(RuntimeError::ProviderUnavailable),
                content("MUST_NOT_POLL"),
            ],
        ),
        (
            "error_after_receipt",
            vec![
                content("committed"),
                receipt(1, 7),
                content("partial"),
                Err(RuntimeError::CostCeilingExceeded),
                receipt(2, 99),
            ],
        ),
        (
            "policy_denied",
            vec![Err(RuntimeError::PolicyDenied {
                reason: "fixture policy".into(),
            })],
        ),
        (
            "cap_denied",
            vec![Err(RuntimeError::CapDenied {
                reason: "fixture verifier".into(),
            })],
        ),
        (
            "approval_required",
            vec![
                content("proposed"),
                Err(RuntimeError::ApprovalRequired {
                    approval_id: "card-1".into(),
                    tool: "file.write".into(),
                    reason: "fixture approval".into(),
                }),
            ],
        ),
        (
            "approval_rejected",
            vec![Err(RuntimeError::ApprovalRejected {
                approval_id: "card-1".into(),
                tool: "file.write".into(),
                reason: "fixture rejection".into(),
            })],
        ),
        (
            "finish_max_tokens",
            vec![
                content("limited"),
                Ok(FusedEvent::Finish(FinishReason::MaxTokens)),
            ],
        ),
        (
            "finish_stop_sequence",
            vec![
                content("sequence\n"),
                Ok(FusedEvent::Finish(FinishReason::StopSequence(
                    "END\n".into(),
                ))),
            ],
        ),
        (
            "finish_error_is_not_stream_error",
            vec![
                content("text"),
                Ok(FusedEvent::Finish(FinishReason::Error(
                    "finish detail".into(),
                ))),
                content("still consumed"),
            ],
        ),
        (
            "finish_tool_use",
            vec![Ok(FusedEvent::Finish(FinishReason::ToolUse(vec![])))],
        ),
        (
            "round_boundaries",
            vec![
                content("first"),
                receipt(1, 5),
                content("second\n"),
                receipt(2, 3),
                receipt(3, 0),
                content("uncommitted"),
            ],
        ),
        (
            "usage_no_receipt",
            vec![
                content("partial"),
                usage(12, 7, Some(99)),
                Err(RuntimeError::ProviderUnavailable),
            ],
        ),
        (
            "usage_with_receipts",
            vec![
                content("first"),
                usage(2, 3, Some(99)),
                receipt(1, 24),
                content("second"),
                usage(5, 7, None),
                receipt(2, 7),
                Ok(FusedEvent::Finish(FinishReason::Stop)),
            ],
        ),
        (
            "usage_after_receipt_error",
            vec![
                usage(2, 3, Some(90)),
                receipt(1, 0),
                Err(RuntimeError::Internal(anyhow::anyhow!("fixture internal"))),
            ],
        ),
        (
            "usage_saturates",
            vec![
                usage(u32::MAX, u32::MAX, Some(u64::MAX)),
                receipt(1, u64::MAX),
                usage(1, 2, Some(10)),
                receipt(2, 12),
            ],
        ),
    ]
}
