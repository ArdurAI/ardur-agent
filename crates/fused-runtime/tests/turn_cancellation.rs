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
    // The probe flips only after dispatch has been observed. If the gate
    // checked before provider.complete, this would cancel without ever
    // entering complete — which the dispatched notify would fail to fire.
    let gone = Arc::new(AtomicBool::new(false));
    let probe = {
        let gone = Arc::clone(&gone);
        move || gone.load(Ordering::SeqCst)
    };

    let dispatched = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(GatedEchoProvider {
        dispatched: Arc::clone(&dispatched),
        release: Arc::clone(&release),
        inner: EchoProvider::new(),
    });

    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = ardur_runtime::SessionId::new();

    let runtime = runtime_builder(provider)
        .receipt_log(&receipt_log)
        .build()
        .expect("runtime builds");

    // Subscribe before polling submit so notify_waiters cannot be missed.
    let dispatched_wait = dispatched.notified();
    tokio::pin!(dispatched_wait);
    let submit = runtime.submit_with_cancellation(
        request_for("mid-flight timeout", &valid_token(), session_id),
        Default::default(),
        Arc::new(probe),
    );
    tokio::pin!(submit);
    tokio::select! {
        biased;
        () = &mut dispatched_wait => {}
        result = &mut submit => panic!("submit finished before dispatch: {result:?}"),
    }
    gone.store(true, Ordering::SeqCst);
    release.notify_one();
    let result = submit.await;
    assert!(
        matches!(result, Err(RuntimeError::TurnCancelled)),
        "gone-by-commit cancels even when present-at-dispatch, got: {result:?}"
    );
    let chain = load_persisted_chain(&receipt_log).expect("chain load succeeds");
    assert!(chain.is_empty());
}

/// Echoes like [`EchoProvider`] but parks in `complete` until `release` is
/// signalled, after announcing entry via `dispatched`. Lets a test prove the
/// probe ran after dispatch, not merely after the future was constructed.
struct GatedEchoProvider {
    dispatched: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    inner: EchoProvider,
}

#[async_trait::async_trait]
impl ardur_provider_runtime::Provider for GatedEchoProvider {
    async fn complete(
        &self,
        req: ardur_provider_runtime::CompletionRequest,
    ) -> Result<ardur_provider_runtime::CompletionResponse, ardur_provider_runtime::ProviderError>
    {
        self.dispatched.notify_waiters();
        self.release.notified().await;
        self.inner.complete(req).await
    }

    fn id(&self) -> ardur_runtime::ProviderId {
        self.inner.id()
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    fn rate_card(&self) -> &ardur_provider_runtime::RateCard {
        self.inner.rate_card()
    }
}
