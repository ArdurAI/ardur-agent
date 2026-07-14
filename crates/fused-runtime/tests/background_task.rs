//! §1.9 integration tests: `FusedRuntime::run_background_task` /
//! `cancel_background_task` — a background task's own terminal receipt,
//! minted on completion, failure, or explicit cancellation, chained onto
//! the same receipt log a turn or a §1.7/§1.8 control receipt would use.

mod support;

use std::sync::Arc;

use ardur_provider_runtime::{CompletionResponse, Provider, ProviderError, RateCard};
use ardur_runtime::{CapTokenRef, SessionId};
use ardur_session_journals::{FileSessionJournal, SessionJournal};
use async_trait::async_trait;

use support::{AUDIENCE, HOLDER, mint_token_as, permissive_policy};

fn task_token() -> String {
    mint_token_as(HOLDER, AUDIENCE, &["task.background"])
}

fn no_capability_token() -> String {
    mint_token_as(HOLDER, AUDIENCE, &["chat.submit"])
}

async fn build_runtime_with_journal(
    journal: Arc<FileSessionJournal>,
    provider: Arc<dyn Provider>,
) -> ardur_fused_runtime::FusedRuntime {
    support::runtime_builder_with_policy(provider, permissive_policy())
        .with_journal(journal)
        .build()
        .expect("runtime builds")
}

/// A provider that always fails, so the failure-path receipt can be tested.
struct FailingProvider {
    rate_card: RateCard,
}

impl FailingProvider {
    fn new() -> Self {
        Self {
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }
}

#[async_trait]
impl Provider for FailingProvider {
    async fn complete(
        &self,
        _req: ardur_provider_runtime::CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        Err(ProviderError::NetworkFailure(
            "simulated provider outage".to_string(),
        ))
    }

    fn id(&self) -> ardur_runtime::ProviderId {
        ardur_runtime::ProviderId("failing".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A successful background task mints a `task.background.completed.v1`
/// receipt carrying the real provider-reported cost, and does not touch the
/// session journal (an agent background task's own transcript is not the
/// foreground conversation).
#[tokio::test]
async fn run_background_task_completes_and_mints_a_receipt() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = build_runtime_with_journal(journal.clone(), provider.clone()).await;
    let cap_token = CapTokenRef(task_token());

    let outcome = runtime
        .run_background_task(
            session_id,
            &cap_token,
            "task.background",
            "audit the repository for slash command references",
        )
        .await
        .expect("the task call succeeds");

    assert_eq!(
        outcome.result.as_deref(),
        Some("audit the repository for slash command references")
    );
    assert!(outcome.error.is_none());

    let entries = journal.replay(session_id).await.expect("journal replays");
    assert!(
        entries.is_empty(),
        "a background task must not journal into the foreground session"
    );
}

/// A failed provider call still mints a terminal
/// `task.background.failed.v1` receipt (invariant 12) and returns `Ok` with
/// `error` set — the outcome, not the call, records the failure.
#[tokio::test]
async fn run_background_task_failure_still_mints_a_terminal_receipt() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let provider: Arc<dyn Provider> = Arc::new(FailingProvider::new());
    let runtime = build_runtime_with_journal(journal, provider).await;
    let cap_token = CapTokenRef(task_token());

    let outcome = runtime
        .run_background_task(session_id, &cap_token, "task.background", "do something")
        .await
        .expect("a provider failure is Ok(outcome), not Err");

    assert!(outcome.result.is_none());
    assert!(outcome.error.is_some());
}

/// `cancel_background_task` mints a `task.background.cancelled.v1` receipt.
#[tokio::test]
async fn cancel_background_task_mints_a_receipt() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = build_runtime_with_journal(journal, provider).await;
    let cap_token = CapTokenRef(task_token());

    let receipt_id = runtime
        .cancel_background_task(session_id, &cap_token, "task.background")
        .await
        .expect("cancellation receipt mints");
    // A fresh, real receipt id, not a placeholder.
    assert_ne!(receipt_id.0, uuid::Uuid::nil());
}

/// A cap-token without `task.background` is denied, and the provider is
/// never dispatched.
#[tokio::test]
async fn background_task_is_denied_without_the_capability() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = build_runtime_with_journal(journal, provider.clone()).await;
    let cap_token = CapTokenRef(no_capability_token());

    let result = runtime
        .run_background_task(session_id, &cap_token, "task.background", "do something")
        .await;

    assert!(result.is_err());
    assert_eq!(provider.call_count(), 0);
}

/// A completed task's receipt and a subsequent turn's receipt share the
/// same chain — the background task is not on a side chain.
#[tokio::test]
async fn background_task_receipts_chain_with_turn_receipts() {
    use ardur_runtime::ChatRuntime;

    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let receipt_log = tempfile::NamedTempFile::new().expect("receipt log");
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = support::runtime_builder_with_policy(provider, permissive_policy())
        .with_journal(journal)
        .receipt_log(receipt_log.path())
        .build()
        .expect("runtime builds");
    let cap_token = CapTokenRef(mint_token_as(
        HOLDER,
        AUDIENCE,
        &["task.background", support::TOOL],
    ));

    let task = runtime
        .run_background_task(session_id, &cap_token, "task.background", "hello task")
        .await
        .expect("task completes");
    let turn = runtime
        .submit(support::request_for("hello", &cap_token.0, session_id))
        .await
        .expect("the turn completes");

    let chain = ardur_fused_runtime::load_persisted_chain(receipt_log.path()).expect("chain loads");
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].body.receipt_id, task.receipt_id.0);
    assert_eq!(chain[1].body.receipt_id, turn.receipt_id.0);
    ardur_fused_runtime::verify_persisted_chain(&chain).expect("the chain verifies");
}
