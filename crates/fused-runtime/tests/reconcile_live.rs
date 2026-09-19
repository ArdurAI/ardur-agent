//! Live projection must finish before a recovery sweep can invent lost text.
mod support;
use ardur_fused_runtime::{ReconciliationAction, ReconciliationError, load_persisted_chain};
use ardur_runtime::{ChatRuntime, SessionId};
use ardur_session_journals::{
    EntryId, FileSessionJournal, JournalEntry, JournalError, SessionJournal,
};
use async_trait::async_trait;
use std::sync::Arc;
use support::*;
use tokio::sync::Notify;

struct ParkAnswer {
    inner: Arc<FileSessionJournal>,
    entered: Notify,
    release: Notify,
}
#[async_trait]
impl SessionJournal for ParkAnswer {
    async fn append(&self, entry: JournalEntry) -> Result<EntryId, JournalError> {
        if matches!(&entry, JournalEntry::AssistantMessage { content, .. } if !content.starts_with("[reconciled]"))
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.append(entry).await
    }
    async fn replay(&self, id: SessionId) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.replay(id).await
    }
    async fn replay_from(
        &self,
        id: SessionId,
        from: EntryId,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.replay_from(id, from).await
    }
    async fn close(&self) -> Result<(), JournalError> {
        self.inner.close().await
    }
    fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }
}

#[tokio::test]
async fn live_reconciliation_cannot_duplicate_a_pending_real_answer() {
    let root = tempdir().unwrap();
    let log = root.path().join("chain.jsonl");
    let owner = SessionId::new();
    let journal = Arc::new(ParkAnswer {
        inner: Arc::new(FileSessionJournal::new(root.path(), owner).unwrap()),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let runtime = runtime_builder(Arc::new(BillingProvider::new(7)))
        .receipt_log(&log)
        .with_journal(journal.clone())
        .build()
        .unwrap();
    let req = request_for("real answer", &valid_token(), SessionId::new());
    let observer = async {
        journal.entered.notified().await;
        assert_eq!(
            load_persisted_chain(&log).unwrap().len(),
            1,
            "the signed receipt must already be durable before the live journal window"
        );
        let before = journal.replay(owner).await.unwrap();
        let sweep = runtime.reconcile_receipts(false).await;
        let after = journal.replay(owner).await.unwrap();
        journal.release.notify_one();
        (sweep, before, after)
    };
    let (result, (sweep, before, during)) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            futures::join!(runtime.submit(req), observer)
        })
        .await
        .unwrap();
    let result = result.unwrap();
    let final_entries = journal.replay(owner).await.unwrap();
    let answers: Vec<_> = final_entries
        .iter()
        .filter_map(|entry| match entry {
            JournalEntry::AssistantMessage {
                content,
                receipt_id,
                ..
            } if *receipt_id == result.receipt_id => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(
        answers.len(),
        1,
        "live recovery must not create a second answer for the committed receipt"
    );
    assert_eq!(answers[0], &result.response.content);
    assert_eq!(during, before, "a deferred sweep must not write");
    assert!(
        matches!(sweep, Err(ReconciliationError::Undecidable { reason })
        if reason == "receipt reconciliation requires idle turn projection")
    );
    assert_eq!(
        runtime.reconcile_receipts(false).await.unwrap().action,
        ReconciliationAction::NoOrphans
    );
    assert_eq!(journal.replay(owner).await.unwrap(), final_entries);
    ardur_fused_runtime::verify_persisted_chain_with_jwks(
        &load_persisted_chain(&log).unwrap(),
        &ardur_receipt::Jwks::from_public_key(&receipt_key().public_key()),
    )
    .unwrap();
}

#[tokio::test]
async fn custom_legacy_completion_verb_recovers_without_recovering_control_receipts() {
    use ardur_receipt::{ReceiptBody, ReceiptSigner, VerbObject};
    use ardur_session_journals::InMemorySessionJournal;
    for (verb, expected) in [
        ("custom.completion.minted.v1", 1),
        ("llm.completion.cancelled.v1", 0),
        ("approval.propose.created.v1", 0),
        ("approval.approve.accepted.v1", 0),
        ("approval.reject.accepted.v1", 0),
        ("tool.grant.allow.v1", 0),
    ] {
        let root = tempdir().unwrap();
        let log = root.path().join("chain.jsonl");
        let owner = SessionId::new();
        let id = uuid::Uuid::new_v4();
        let signed = ReceiptSigner::sign(
            ReceiptBody {
                receipt_id: id,
                parent_hash: None,
                verb: VerbObject::new(verb).unwrap(),
                issued_at: ardur_receipt::UnixTsMillis(NOW_MS),
                subject: ardur_receipt::HolderId(HOLDER.into()),
                cap_token_id: ardur_receipt::TokenId(uuid::Uuid::new_v4()),
                payload_digest: ardur_receipt::Sha256Digest::of(b"legacy evidence"),
                session_id: Some(owner.0),
                cost: Default::default(),
                tool_calls: vec![],
                provider: None,
            },
            &receipt_key(),
        )
        .unwrap();
        std::fs::write(&log, format!("{}\n", signed.jws_compact())).unwrap();
        let original = std::fs::read(&log).unwrap();
        let journal = Arc::new(InMemorySessionJournal::new(owner));
        let runtime = runtime_builder(Arc::new(EchoProvider::new()))
            .verb(VerbObject::new(verb).unwrap())
            .receipt_log(&log)
            .with_journal(journal.clone())
            .build()
            .unwrap();
        assert_eq!(
            runtime
                .reconcile_receipts(true)
                .await
                .unwrap()
                .orphan_receipt_count(),
            expected,
            "{verb}"
        );
        assert!(journal.replay(owner).await.unwrap().is_empty());
        assert_eq!(
            runtime
                .reconcile_receipts(false)
                .await
                .unwrap()
                .orphan_receipt_count(),
            expected,
            "{verb}"
        );
        let entries = journal.replay(owner).await.unwrap();
        assert_eq!(entries.len(), expected, "{verb}");
        if expected != 0 {
            assert!(
                matches!(&entries[0], JournalEntry::AssistantMessage { content, receipt_id, .. }
                if content.starts_with("[reconciled]") && receipt_id.0 == id)
            );
        }
        assert_eq!(
            runtime.reconcile_receipts(false).await.unwrap().action,
            ReconciliationAction::NoOrphans
        );
        assert_eq!(journal.replay(owner).await.unwrap(), entries);
        assert_eq!(std::fs::read(&log).unwrap(), original);
    }
}
