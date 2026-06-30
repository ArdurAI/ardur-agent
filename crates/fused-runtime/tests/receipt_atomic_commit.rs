use std::sync::Arc;

use ardur_fused_runtime::{ReceiptChainError, load_persisted_chain};
use ardur_receipt::{ReceiptBody, ReceiptSigner, Sha256Digest, VerbObject};
use ardur_runtime::{ChatRuntime, SessionId};
use ardur_session_journals::{EntryId, JournalEntry, JournalError, SessionJournal};
use async_trait::async_trait;
use futures::StreamExt as _;

mod support;
use support::{EchoProvider, request_for, runtime_builder, valid_token};

struct FailingAppendJournal {
    session_id: SessionId,
}

#[async_trait]
impl SessionJournal for FailingAppendJournal {
    async fn append(&self, _entry: JournalEntry) -> Result<EntryId, JournalError> {
        Err(JournalError::Io(std::io::Error::other(
            "injected append failure",
        )))
    }

    async fn replay(&self, session_id: SessionId) -> Result<Vec<JournalEntry>, JournalError> {
        if session_id == self.session_id {
            Ok(Vec::new())
        } else {
            Err(JournalError::SessionNotFound(session_id))
        }
    }

    async fn replay_from(
        &self,
        session_id: SessionId,
        _from: EntryId,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        self.replay(session_id).await
    }

    async fn close(&self) -> Result<(), JournalError> {
        Ok(())
    }

    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

#[tokio::test]
async fn journal_append_failure_does_not_persist_receipt() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = SessionId::new();
    let provider = Arc::new(EchoProvider::new());
    let journal = Arc::new(FailingAppendJournal { session_id });

    let runtime = runtime_builder(provider)
        .with_journal(journal)
        .receipt_log(&receipt_log)
        .build()
        .expect("runtime builds");

    let result = runtime
        .submit(request_for("atomic commit", &valid_token(), session_id))
        .await;

    assert!(result.is_err(), "journal failure must fail the turn commit");
    let chain = load_persisted_chain(&receipt_log).expect("chain load succeeds");
    assert!(
        chain.is_empty(),
        "receipt must not be durable without its journal entry"
    );
}

#[tokio::test]
async fn receipt_persist_failure_rolls_back_journal_entries() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log_is_directory = root.path().join("missing-parent").join("receipts.jsonl");
    let session_id = SessionId::new();
    let provider = Arc::new(EchoProvider::new());
    let journal = Arc::new(ardur_session_journals::InMemorySessionJournal::new(
        session_id,
    ));

    let runtime = runtime_builder(provider)
        .with_journal(journal.clone())
        .receipt_log(&receipt_log_is_directory)
        .build()
        .expect("runtime builds even before opening receipt log for append");

    let result = runtime
        .submit(request_for(
            "receipt persist fails",
            &valid_token(),
            session_id,
        ))
        .await;

    assert!(
        result.is_err(),
        "receipt persistence failure must fail the turn"
    );
    let entries = journal
        .replay(session_id)
        .await
        .expect("journal replay succeeds");
    assert!(
        entries.is_empty(),
        "two-phase commit rolls back journal entries when the receipt cannot persist: {entries:?}"
    );
}

#[tokio::test]
async fn stream_journal_append_failure_does_not_persist_receipt() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = SessionId::new();
    let provider = Arc::new(EchoProvider::new());
    let journal = Arc::new(FailingAppendJournal { session_id });

    let runtime = runtime_builder(provider)
        .with_journal(journal)
        .receipt_log(&receipt_log)
        .build()
        .expect("runtime builds");

    let events = Box::pin(runtime.stream(request_for(
        "atomic stream commit",
        &valid_token(),
        session_id,
    )))
    .collect::<Vec<_>>()
    .await;

    assert!(
        events.iter().any(Result::is_err),
        "stream journal failure must fail the turn commit: {events:?}"
    );
    let chain = load_persisted_chain(&receipt_log).expect("chain load succeeds");
    assert!(
        chain.is_empty(),
        "stream receipt must not be durable without its journal entry"
    );
}

#[tokio::test]
async fn stream_receipt_persist_failure_rolls_back_journal_entries() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log_is_directory = root.path().join("missing-parent").join("receipts.jsonl");
    let session_id = SessionId::new();
    let provider = Arc::new(EchoProvider::new());
    let journal = Arc::new(ardur_session_journals::InMemorySessionJournal::new(
        session_id,
    ));

    let runtime = runtime_builder(provider)
        .with_journal(journal.clone())
        .receipt_log(&receipt_log_is_directory)
        .build()
        .expect("runtime builds even before opening receipt log for append");

    let events = Box::pin(runtime.stream(request_for(
        "stream receipt persist fails",
        &valid_token(),
        session_id,
    )))
    .collect::<Vec<_>>()
    .await;

    assert!(
        events.iter().any(Result::is_err),
        "stream receipt persistence failure must fail the turn"
    );
    let entries = journal
        .replay(session_id)
        .await
        .expect("journal replay succeeds");
    assert!(
        entries.is_empty(),
        "stream two-phase commit rolls back journal entries when the receipt cannot persist: {entries:?}"
    );
}

#[test]
fn boot_refuses_broken_receipt_chain() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let key = support::receipt_key();

    let make_body = |content: &[u8]| ReceiptBody {
        receipt_id: uuid::Uuid::new_v4(),
        parent_hash: None,
        verb: VerbObject::new("llm.completion.minted.v1").expect("verb"),
        issued_at: ardur_receipt::UnixTsMillis(support::NOW_MS),
        subject: ardur_receipt::HolderId(support::HOLDER.to_string()),
        cap_token_id: ardur_receipt::TokenId("cap".to_string()),
        payload_digest: Sha256Digest::of(content),
        cost: ardur_receipt::CostTuple {
            tokens_in: 0,
            tokens_out: 0,
            cents: 0,
            wall_ms: 0,
            attention_score: 0.0,
        },
        tool_calls: Vec::new(),
        provider: Some("test".to_string()),
    };

    let first = ReceiptSigner::sign(make_body(b"one"), &key).expect("sign first");
    let second =
        ReceiptSigner::sign(make_body(b"two"), &key).expect("sign second with bad genesis parent");
    std::fs::write(
        &receipt_log,
        format!("{}\n{}\n", first.jws_compact(), second.jws_compact()),
    )
    .expect("write broken chain");

    let err = match runtime_builder(Arc::new(EchoProvider::new()))
        .receipt_log(&receipt_log)
        .build()
    {
        Ok(_) => panic!("boot must reject a broken persisted chain"),
        Err(err) => err,
    };

    assert!(
        matches!(err, ReceiptChainError::BrokenChain { at: 1 }),
        "expected broken chain at second receipt, got {err:?}"
    );
}
