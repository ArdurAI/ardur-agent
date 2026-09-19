//! Mid-turn journal failure: definite modern answer markers recover, but unknown
//! projections stay quarantined; authenticated legacy recovery remains idempotent.
//!
//! A caught task panic plus destruction of the runtime models the supported
//! restart boundary here, not a kernel failure or arbitrary power-loss proof.
//! Modern turns append CostFinalized before UserMessage and AssistantMessage.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ardur_e2e_tests::fixtures;
use ardur_fused_runtime::{
    ReconciliationAction, load_persisted_chain, verify_persisted_chain_with_jwks,
};
use ardur_receipt::{Jwks, ReceiptBody, ReceiptSigner, Sha256Digest, VerbObject};
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, ReceiptId, SessionId, SubmitRequest};
use ardur_session_journals::{
    EntryId, FileSessionJournal, JournalEntry, JournalError, SessionJournal,
};
use async_trait::async_trait;

mod support;
use support::EchoProvider;

struct CrashAtAppend {
    inner: Arc<FileSessionJournal>,
    seen: AtomicU64,
    crash_at: u64,
}
#[async_trait]
impl SessionJournal for CrashAtAppend {
    async fn append(&self, entry: JournalEntry) -> Result<EntryId, JournalError> {
        let index = self.seen.fetch_add(1, Ordering::SeqCst);
        assert_ne!(
            index, self.crash_at,
            "controlled panic after durable receipt, before projection acknowledgement"
        );
        self.inner.append(entry).await
    }
    async fn replay(&self, session: SessionId) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.replay(session).await
    }
    async fn replay_from(
        &self,
        session: SessionId,
        from: EntryId,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.replay_from(session, from).await
    }
    async fn close(&self) -> Result<(), JournalError> {
        self.inner.close().await
    }
    fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }
}
fn request(content: &str, session: SessionId) -> SubmitRequest {
    SubmitRequest {
        messages: vec![ChatMessage::user(content)],
        cap_token: CapTokenRef(fixtures::dev_valid_cap_token()),
        session_id: session,
        requested_provider: None,
    }
}
fn verify(path: &Path) {
    let chain = load_persisted_chain(path).unwrap();
    verify_persisted_chain_with_jwks(
        &chain,
        &Jwks::from_public_key(&fixtures::dev_receipt_key().public_key()),
    )
    .unwrap();
}

#[tokio::test]
async fn journal_mid_turn_crash_retains_unresolved_settlement_at_boot() {
    let root = fixtures::temp_session_root();
    let receipt_log = root.path().join("receipts.jsonl");
    let session = SessionId::new();
    let local = tokio::task::LocalSet::new();
    let (journal_before, chain_before) = local
        .run_until(async {
            let provider = Arc::new(EchoProvider::new());
            let journal = Arc::new(FileSessionJournal::new(root.path(), session).unwrap());
            let crashing = Arc::new(CrashAtAppend {
                inner: journal.clone(),
                seen: AtomicU64::new(0),
                crash_at: 2 * 3,
            });
            let runtime = Arc::new(
                fixtures::fused_builder(provider.clone())
                    .with_journal(crashing.clone())
                    .receipt_log(&receipt_log)
                    .build()
                    .unwrap(),
            );
            for prompt in ["turn one", "turn two"] {
                runtime.submit(request(prompt, session)).await.unwrap();
            }
            let before = journal.replay(session).await.unwrap();
            assert_eq!(before.len(), 2 * 3);
            assert_eq!(
                before
                    .iter()
                    .filter(|entry| matches!(entry, JournalEntry::CostFinalized { .. }))
                    .count(),
                2
            );
            let bytes = std::fs::read(journal.path()).unwrap();
            let previous = load_persisted_chain(&receipt_log).unwrap();
            assert_eq!(previous.len(), 2);
            let tail = Sha256Digest::of(previous.last().unwrap().jws_compact.as_bytes());
            let owned = runtime.clone();
            let result = tokio::task::spawn_local(async move {
                owned.submit(request("turn three", session)).await
            })
            .await;
            assert!(result.unwrap_err().is_panic());
            assert_eq!(provider.call_count(), 3);
            assert_eq!(journal.replay(session).await.unwrap(), before);
            assert_eq!(std::fs::read(journal.path()).unwrap(), bytes);
            let chain = load_persisted_chain(&receipt_log).unwrap();
            assert_eq!(chain.len(), 3);
            assert_eq!(chain[2].body.parent_hash, Some(tail));
            verify(&receipt_log);
            assert!(!runtime.settlement_supervisor().status().turns.is_empty());
            // Dropping the last live owner is deliberate restart simulation, not a
            // successful supervised close. The unresolved intent is already durable.
            (before, std::fs::read(&receipt_log).unwrap())
        })
        .await;
    let journal = Arc::new(FileSessionJournal::new(root.path(), session).unwrap());
    let provider = Arc::new(EchoProvider::new());
    let (runtime, report) = fixtures::fused_builder(provider.clone())
        .with_journal(journal.clone())
        .receipt_log(&receipt_log)
        .build_reconciled()
        .await
        .unwrap();
    // The final receipt binding is definite even though its cost projection is
    // ambiguous. Recover only the lost answer marker; keep economic quarantine.
    assert_eq!(report.orphan_receipt_count(), 1);
    let supervisor = runtime.settlement_supervisor();
    assert!(supervisor.status().boot_problem.is_some());
    let budget = runtime.remaining_budget(&fixtures::gate_holder()).await;
    let _ = supervisor.drain_pending(journal.as_ref()).await;
    assert!(supervisor.status().boot_problem.is_some());
    assert!(
        runtime
            .submit(request("still quarantined", session))
            .await
            .is_err()
    );
    assert_eq!(provider.call_count(), 0);
    assert_eq!(
        runtime.remaining_budget(&fixtures::gate_holder()).await,
        budget
    );
    let after = journal.replay(session).await.unwrap();
    assert_eq!(&after[..journal_before.len()], journal_before.as_slice());
    assert_eq!(after.len(), journal_before.len() + 1);
    let final_id = load_persisted_chain(&receipt_log).unwrap()[2]
        .body
        .receipt_id;
    assert!(
        matches!(after.last().unwrap(), JournalEntry::AssistantMessage { content, receipt_id, .. }
        if content.starts_with("[reconciled]") && receipt_id.0 == final_id)
    );
    assert_eq!(
        runtime
            .reconcile_receipts(false)
            .await
            .unwrap()
            .orphan_receipt_count(),
        0
    );
    assert_eq!(std::fs::read(&receipt_log).unwrap(), chain_before);
    verify(&receipt_log);
    assert!(supervisor.try_close().is_err());
}

#[tokio::test]
async fn legacy_orphan_reconciles_once_and_preserves_chain() {
    use std::io::Write;
    let root = fixtures::temp_session_root();
    let receipt_log = root.path().join("legacy-receipts.jsonl");
    let session = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(root.path(), session).unwrap());
    let key = fixtures::dev_receipt_key();
    let mut parent = None;
    let mut ids = Vec::new();
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&receipt_log).unwrap();
    // Real signed legacy artifacts have no settlement-snapshot namespace.
    // Nothing is deleted from a modern turn to manufacture this distinction.
    for (index, prompt) in ["one", "two", "missing transcript"].iter().enumerate() {
        let id = uuid::Uuid::new_v4();
        let signed = ReceiptSigner::sign(
            ReceiptBody {
                receipt_id: id,
                parent_hash: parent,
                verb: VerbObject::new("llm.completion.minted.v1").unwrap(),
                issued_at: ardur_receipt::UnixTsMillis(fixtures::NOW_MS),
                subject: ardur_receipt::HolderId(fixtures::TEST_HOLDER.into()),
                cap_token_id: ardur_receipt::TokenId(uuid::Uuid::new_v4()),
                payload_digest: Sha256Digest::of(prompt.as_bytes()),
                session_id: Some(session.0),
                cost: ardur_receipt::CostTuple {
                    tokens_in: 0,
                    tokens_out: 0,
                    cents: 0,
                    wall_ms: 0,
                    attention_score: 0,
                },
                tool_calls: vec![],
                provider: Some("legacy-fixture".into()),
            },
            &key,
        )
        .unwrap();
        writeln!(file, "{}", signed.jws_compact()).unwrap();
        file.sync_all().unwrap();
        parent = Some(Sha256Digest::of(signed.jws_compact().as_bytes()));
        ids.push(id);
        if index < 2 {
            journal
                .append(JournalEntry::UserMessage {
                    content: (*prompt).into(),
                    at: ardur_cost_gate::UnixTsMillis(fixtures::NOW_MS),
                })
                .await
                .unwrap();
            journal
                .append(JournalEntry::AssistantMessage {
                    content: (*prompt).into(),
                    at: ardur_cost_gate::UnixTsMillis(fixtures::NOW_MS),
                    receipt_id: ReceiptId(id),
                })
                .await
                .unwrap();
        }
    }
    drop(file);
    verify(&receipt_log);
    let chain_before = std::fs::read(&receipt_log).unwrap();
    let provider = Arc::new(EchoProvider::new());
    let (runtime, report) = fixtures::fused_builder(provider.clone())
        .with_journal(journal.clone())
        .receipt_log(&receipt_log)
        .build_reconciled()
        .await
        .unwrap();
    assert_eq!(report.orphan_receipt_ids, vec![ids[2]]);
    assert_eq!(
        report.action,
        ReconciliationAction::AppendedSyntheticJournal { count: 1 }
    );
    assert_eq!(std::fs::read(&receipt_log).unwrap(), chain_before);
    let after = journal.replay(session).await.unwrap();
    assert_eq!(after.len(), 2 * 2 + 1);
    assert!(
        matches!(after.last().unwrap(), JournalEntry::AssistantMessage { content, receipt_id, .. } if content.contains("[reconciled]") && receipt_id.0 == ids[2])
    );
    assert_eq!(
        runtime
            .reconcile_receipts(false)
            .await
            .unwrap()
            .orphan_receipt_count(),
        0
    );
    assert_eq!(journal.replay(session).await.unwrap(), after);
    runtime.submit(request("new turn", session)).await.unwrap();
    assert_eq!(provider.call_count(), 1);
    let chain = load_persisted_chain(&receipt_log).unwrap();
    assert_eq!(chain.len(), 4);
    assert_eq!(chain[3].body.parent_hash, parent);
    verify(&receipt_log);
}
