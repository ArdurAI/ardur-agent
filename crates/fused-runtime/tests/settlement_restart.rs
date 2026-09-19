//! Narrow fresh-process proof, not missing-transcript reconstruction.
mod support;
use ardur_cost_gate::{CostEnvelope, CostTuple};
use ardur_fused_runtime::{FusedRuntime, FusedRuntimeBuilder};
use ardur_provider_runtime::ModelId;
use ardur_receipt::Es256SigningKey;
use ardur_runtime::{ChatRuntime, SessionId};
use ardur_session_journals::{
    EntryId, FileSessionJournal, JournalEntry, JournalError, ProjectionOutcome, SessionJournal,
};
use async_trait::async_trait;
use std::{path::Path, sync::Arc};

struct UnknownProjection(Arc<FileSessionJournal>);
#[async_trait]
impl SessionJournal for UnknownProjection {
    async fn append(&self, entry: JournalEntry) -> Result<EntryId, JournalError> {
        self.0.append(entry).await
    }
    async fn append_settlement(&self, _: uuid::Uuid, _: JournalEntry) -> ProjectionOutcome {
        ProjectionOutcome::Unknown(JournalError::Io(std::io::Error::other(
            "lost projection acknowledgement",
        )))
    }
    async fn replay(&self, id: SessionId) -> Result<Vec<JournalEntry>, JournalError> {
        self.0.replay(id).await
    }
    async fn replay_from(
        &self,
        id: SessionId,
        from: EntryId,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        self.0.replay_from(id, from).await
    }
    async fn close(&self) -> Result<(), JournalError> {
        self.0.close().await
    }
    fn session_id(&self) -> &SessionId {
        self.0.session_id()
    }
}
async fn build(
    root: &Path,
    journal: Arc<dyn SessionJournal>,
    provider: Arc<support::BillingProvider>,
) -> FusedRuntime {
    let key =
        Es256SigningKey::from_pkcs8_pem(&std::fs::read_to_string(root.join("key.pem")).unwrap())
            .unwrap();
    FusedRuntimeBuilder::new(
        support::cap_root(),
        support::permissive_policy(),
        provider,
        key,
        ModelId::new(support::TEST_MODEL),
    )
    .require_durable_settlements()
    .audience(support::AUDIENCE)
    .tool(support::TOOL)
    .clock(support::manual_clock())
    .provision_budget(support::gate_holder(), CostTuple::cents(10))
    .projected_envelope(CostEnvelope {
        cents_max: 10,
        ..Default::default()
    })
    .with_journal(journal)
    .receipt_log(root.join("receipts.jsonl"))
    .build_reconciled()
    .await
    .unwrap()
    .0
}

#[tokio::test]
async fn fresh_process_preserves_terminal_evidence_without_replaying_old_budget() {
    const ENV: &str = "ARDUR_SETTLEMENT_RESTART_FIXTURE";
    if let Some(root) = std::env::var_os(ENV) {
        let root = std::path::PathBuf::from(root);
        let unknown = root.join("unknown").exists();
        let session = SessionId(uuid::Uuid::nil());
        let journal = Arc::new(FileSessionJournal::new(root.join("journals"), session).unwrap());
        let before = journal.replay(session).await.unwrap();
        let chain_before = std::fs::read(root.join("receipts.jsonl")).unwrap_or_default();
        let provider = Arc::new(support::BillingProvider::new(9));
        let runtime = build(&root, journal.clone(), provider.clone()).await;
        assert_eq!(
            runtime
                .remaining_budget(&support::gate_holder())
                .await
                .unwrap()
                .cents,
            10,
            "old epoch must never debit fresh budget"
        );
        let supervisor = runtime.settlement_supervisor();
        assert_eq!(supervisor.status().boot_problem.is_some(), unknown);
        // Healthy journal/drain must NOT clear old ambiguous application state.
        let _ = supervisor.drain_pending(journal.as_ref()).await;
        assert_eq!(supervisor.status().boot_problem.is_some(), unknown);
        if unknown {
            assert!(
                runtime
                    .submit(support::request_for(
                        "blocked",
                        &support::valid_token(),
                        session
                    ))
                    .await
                    .is_err()
            );
            assert!(supervisor.clone().try_close().is_err());
        } else {
            assert!(supervisor.clone().try_close().is_ok());
        }
        assert_eq!(provider.call_count(), 0);
        assert_eq!(
            runtime
                .remaining_budget(&support::gate_holder())
                .await
                .unwrap()
                .cents,
            10
        );
        let after = journal.replay(session).await.unwrap();
        assert_eq!(&after[..before.len()], before.as_slice());
        assert_eq!(after.len(), before.len() + usize::from(unknown));
        if unknown {
            let id = ardur_fused_runtime::load_persisted_chain(root.join("receipts.jsonl"))
                .unwrap()[0]
                .body
                .receipt_id;
            assert!(
                matches!(after.last().unwrap(), JournalEntry::AssistantMessage { content, receipt_id, .. }
                if content.starts_with("[reconciled]") && receipt_id.0 == id)
            );
        }
        assert_eq!(
            runtime
                .reconcile_receipts(false)
                .await
                .unwrap()
                .orphan_receipt_count(),
            0
        );
        assert_eq!(
            std::fs::read(root.join("receipts.jsonl")).unwrap_or_default(),
            chain_before
        );
        return;
    }
    for unknown in [false, true] {
        let dir = support::tempdir().unwrap();
        let root = dir.path();
        // Test-only generated key, private temp directory, never printed.
        std::fs::write(
            root.join("key.pem"),
            Es256SigningKey::generate().to_pkcs8_pem().unwrap(),
        )
        .unwrap();
        let session = SessionId(uuid::Uuid::nil());
        let journal = Arc::new(FileSessionJournal::new(root.join("journals"), session).unwrap());
        let projection: Arc<dyn SessionJournal> = if unknown {
            std::fs::write(root.join("unknown"), b"projection ack lost").unwrap();
            Arc::new(UnknownProjection(journal.clone()))
        } else {
            journal.clone()
        };
        let runtime = build(root, projection, Arc::new(support::BillingProvider::new(9))).await;
        let result = runtime
            .submit(support::request_for(
                "local",
                &support::valid_token(),
                session,
            ))
            .await;
        assert_eq!(result.is_err(), unknown);
        assert_eq!(
            runtime
                .remaining_budget(&support::gate_holder())
                .await
                .unwrap()
                .cents,
            1,
            "real nonzero debit, even if acknowledgement unknown"
        );
        let supervisor = runtime.settlement_supervisor();
        if !unknown {
            assert_eq!(supervisor.drain_pending(journal.as_ref()).await.unwrap(), 0);
            assert!(supervisor.clone().try_close().is_ok());
        }
        // Deliberate process-owner teardown models the documented restart limit:
        // evidence is durable, but an ambiguous prior epoch is quarantined.
        drop(runtime);
        drop(supervisor);
        drop(journal);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fresh_process_preserves_terminal_evidence_without_replaying_old_budget",
                "--nocapture",
            ])
            .env(ENV, root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fresh verifier failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
