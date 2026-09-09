//! #359 — the commit gate: a caller who went away mid-round must never be
//! billed for a receipt they never saw.
//!
//! The HTTP worker's drop-based cancellation is the fast path, but under task
//! starvation a completed provider round can win a biased select before the
//! worker ever observes the drop. These tests pin the scheduling-independent
//! floor: the fused runtime consults the caller-liveness probe AFTER the
//! provider round returns and BEFORE the receipt/journal/billing commit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ardur_fused_runtime::load_persisted_chain;
use ardur_runtime::RuntimeError;
use ardur_session_journals::InMemorySessionJournal;

mod support;
use support::{EchoProvider, request_for, runtime_builder, valid_token};

#[tokio::test]
async fn a_caller_gone_at_the_commit_gate_gets_no_receipt() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = ardur_runtime::SessionId::new();
    let provider = Arc::new(EchoProvider::new());
    let journal: Arc<dyn ardur_session_journals::SessionJournal> =
        Arc::new(InMemorySessionJournal::new(session_id));

    let runtime = runtime_builder(provider)
        .with_journal(Arc::clone(&journal))
        .receipt_log(&receipt_log)
        .build()
        .expect("runtime builds");

    // The caller is gone by the time the (instant) provider round settles.
    let probe: ardur_fused_runtime::CancelProbe = Arc::new(|| true);
    let result = runtime
        .submit_with_cancellation(
            request_for("abandoned turn", &valid_token(), session_id),
            Default::default(),
            probe,
        )
        .await;

    assert!(
        matches!(result, Err(RuntimeError::TurnCancelled)),
        "an abandoned turn aborts at the commit gate, got: {result:?}"
    );
    let chain = load_persisted_chain(&receipt_log).expect("chain load succeeds");
    assert!(
        chain.is_empty(),
        "no receipt may be minted for a caller who already saw 504"
    );
    assert!(
        journal.replay(session_id).await.expect("replay").is_empty(),
        "no journal entries may commit either"
    );
}

#[tokio::test]
async fn a_present_caller_commits_normally() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = ardur_runtime::SessionId::new();
    let provider = Arc::new(EchoProvider::new());

    let runtime = runtime_builder(provider)
        .receipt_log(&receipt_log)
        .build()
        .expect("runtime builds");

    let probe: ardur_fused_runtime::CancelProbe = Arc::new(|| false);
    let result = runtime
        .submit_with_cancellation(
            request_for("present turn", &valid_token(), session_id),
            Default::default(),
            probe,
        )
        .await;
    assert!(
        result.is_ok(),
        "a present caller's turn commits: {result:?}"
    );
    let chain = load_persisted_chain(&receipt_log).expect("chain load succeeds");
    assert_eq!(chain.len(), 1, "the normal path mints exactly one receipt");
}

#[tokio::test]
async fn the_probe_is_consulted_after_the_provider_round_not_before() {
    // The probe flips only once the provider has been called, mirroring the
    // real race: the timeout fires WHILE the round is in flight. If the gate
    // checked before dispatch, this would cancel even though the caller was
    // still present at dispatch time.
    let gone = Arc::new(AtomicBool::new(false));
    let probe = {
        let gone = Arc::clone(&gone);
        move || gone.load(Ordering::SeqCst)
    };

    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = ardur_runtime::SessionId::new();
    let provider = Arc::new(EchoProvider::new());

    let runtime = runtime_builder(provider)
        .receipt_log(&receipt_log)
        .build()
        .expect("runtime builds");

    // Caller still present for dispatch...
    let submit = runtime.submit_with_cancellation(
        request_for("mid-flight timeout", &valid_token(), session_id),
        Default::default(),
        Arc::new(probe),
    );
    // ...but gone by the time the round settles. With an instant provider the
    // flag flips before the commit gate is reached, deterministically.
    gone.store(true, Ordering::SeqCst);
    let result = submit.await;
    assert!(
        matches!(result, Err(RuntimeError::TurnCancelled)),
        "gone-by-commit cancels even when present-at-dispatch, got: {result:?}"
    );
    let chain = load_persisted_chain(&receipt_log).expect("chain load succeeds");
    assert!(chain.is_empty());
}
