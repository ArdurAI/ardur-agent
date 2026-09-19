//! The public one-shot CLI writer and runtime must own the same stable lease.
use ardur_fused_runtime::{
    ReceiptChainError, load_persisted_chain, mint_control_receipt, verify_persisted_chain_with_jwks,
};
use ardur_receipt::{CostTuple, HolderId, Jwks, ReceiptBody, Sha256Digest, TokenId, VerbObject};
use ardur_runtime::{ChatRuntime, SessionId};
use std::{path::Path, sync::Arc};
mod support;
use support::*;

fn control(path: &Path) -> Result<ReceiptBody, ReceiptChainError> {
    mint_control_receipt(
        path,
        &receipt_key(),
        VerbObject::new("approval.approve.accepted.v1").unwrap(),
        Sha256Digest::of(b"control"),
        HolderId(HOLDER.into()),
        TokenId(uuid::Uuid::new_v4()),
        None,
        CostTuple::default(),
        NOW_MS,
    )
}

#[tokio::test]
async fn standalone_writer_refuses_live_runtime_and_retained_supervisor_then_chains_after_release()
{
    let root = tempdir().unwrap();
    let log = root.path().join("chain.jsonl");
    let runtime = runtime_builder(Arc::new(EchoProvider::new()))
        .receipt_log(&log)
        .build()
        .unwrap();
    runtime
        .submit(request_for("first", &valid_token(), SessionId::new()))
        .await
        .unwrap();
    let before = std::fs::read(&log).unwrap();
    let supervisor = runtime.settlement_supervisor();
    assert!(
        matches!(
            control(&log),
            Err(ReceiptChainError::Writer(
                ardur_session_journals::settlement::SettlementStoreError::WriterBusy
            ))
        ),
        "standalone writer must refuse before mutating a live runtime's log"
    );
    assert_eq!(std::fs::read(&log).unwrap(), before);
    assert!(
        runtime_builder(Arc::new(EchoProvider::new()))
            .receipt_log(&log)
            .build()
            .is_err()
    );
    drop(runtime);
    assert!(
        matches!(
            control(&log),
            Err(ReceiptChainError::Writer(
                ardur_session_journals::settlement::SettlementStoreError::WriterBusy
            ))
        ),
        "retaining supervisor retains the writer lease"
    );
    assert_eq!(std::fs::read(&log).unwrap(), before);
    assert!(supervisor.try_close().is_ok());
    let appended = control(&log).unwrap();
    let chain = load_persisted_chain(&log).unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(
        appended.parent_hash,
        Some(Sha256Digest::of(chain[0].jws_compact.as_bytes()))
    );
    let runtime = runtime_builder(Arc::new(EchoProvider::new()))
        .receipt_log(&log)
        .build()
        .unwrap();
    runtime
        .submit(request_for("next", &valid_token(), SessionId::new()))
        .await
        .unwrap();
    let chain = load_persisted_chain(&log).unwrap();
    assert_eq!(chain.len(), 3);
    verify_persisted_chain_with_jwks(&chain, &Jwks::from_public_key(&receipt_key().public_key()))
        .unwrap();
}

#[test]
fn standalone_lease_precedes_runtime_preflight_and_refuses_incompatible_signer() {
    use ardur_fused_runtime::ControlReceiptWriter;
    let root = tempdir().unwrap();
    let log = root.path().join("chain.jsonl");
    let writer = ControlReceiptWriter::open(&log, &receipt_key()).unwrap();
    assert!(
        runtime_builder(Arc::new(EchoProvider::new()))
            .receipt_log(&log)
            .build()
            .is_err()
    );
    assert!(
        !log.exists(),
        "refused runtime must not create a receipt log before acquiring ownership"
    );
    assert!(matches!(
        control(&log),
        Err(ReceiptChainError::Writer(
            ardur_session_journals::settlement::SettlementStoreError::WriterBusy
        ))
    ));
    drop(writer);
    control(&log).unwrap();
    let before = std::fs::read(&log).unwrap();
    assert!(ControlReceiptWriter::open(&log, &ardur_receipt::Es256SigningKey::generate()).is_err());
    assert_eq!(std::fs::read(&log).unwrap(), before);
    control(&log).unwrap();
    verify_persisted_chain_with_jwks(
        &load_persisted_chain(&log).unwrap(),
        &Jwks::from_public_key(&receipt_key().public_key()),
    )
    .unwrap();
}

#[tokio::test]
async fn same_session_standalone_control_receipt_never_becomes_an_answer() {
    use ardur_fused_runtime::ControlReceiptWriter;
    use ardur_session_journals::{InMemorySessionJournal, SessionJournal};
    let root = tempdir().unwrap();
    let log = root.path().join("chain.jsonl");
    let owner = SessionId::new();
    ControlReceiptWriter::open(&log, &receipt_key())
        .unwrap()
        .mint(
            VerbObject::new("approval.approve.accepted.v1").unwrap(),
            Sha256Digest::of(b"control"),
            HolderId(HOLDER.into()),
            TokenId(uuid::Uuid::new_v4()),
            Some(owner.0),
            CostTuple::default(),
            NOW_MS,
        )
        .unwrap();
    let journal = Arc::new(InMemorySessionJournal::new(owner));
    let (_, report) = runtime_builder(Arc::new(EchoProvider::new()))
        .receipt_log(&log)
        .with_journal(journal.clone())
        .build_reconciled()
        .await
        .unwrap();
    assert_eq!(report.orphan_receipt_count(), 0);
    assert!(journal.replay(owner).await.unwrap().is_empty());
    verify_persisted_chain_with_jwks(
        &load_persisted_chain(&log).unwrap(),
        &Jwks::from_public_key(&receipt_key().public_key()),
    )
    .unwrap();
}

fn failed_authentication_does_not_bind_legacy_log(runtime: bool) {
    use ardur_fused_runtime::ControlReceiptWriter;
    use ardur_receipt::{Es256SigningKey, ReceiptSigner, UnixTsMillis};
    for existing_empty_namespace in [false, true] {
        let root = tempdir().unwrap();
        let log = root.path().join("chain.jsonl");
        let namespace = root.path().join("chain.jsonl.settlements");
        if existing_empty_namespace {
            std::fs::create_dir(&namespace).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&namespace, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
        }
        let correct_key = Es256SigningKey::generate();
        let signed = ReceiptSigner::sign(
            ReceiptBody {
                receipt_id: uuid::Uuid::new_v4(),
                parent_hash: None,
                verb: VerbObject::new("approval.approve.accepted.v1").unwrap(),
                issued_at: UnixTsMillis(NOW_MS),
                subject: HolderId(HOLDER.into()),
                cap_token_id: TokenId(uuid::Uuid::new_v4()),
                payload_digest: Sha256Digest::of(b"genuine legacy receipt"),
                session_id: None,
                cost: CostTuple::default(),
                tool_calls: vec![],
                provider: None,
            },
            &correct_key,
        )
        .unwrap();
        std::fs::write(&log, format!("{}\n", signed.jws_compact())).unwrap();
        let before = std::fs::read(&log).unwrap();
        let error = if runtime {
            runtime_builder(Arc::new(EchoProvider::new()))
                .receipt_log(&log)
                .build()
                .err()
                .unwrap()
        } else {
            ControlReceiptWriter::open(&log, &receipt_key())
                .err()
                .unwrap()
        };
        assert!(
            matches!(error, ReceiptChainError::InvalidSignature { .. }),
            "{error:?}"
        );
        assert_eq!(std::fs::read(&log).unwrap(), before);
        assert!(
            !namespace.join("format.json").exists(),
            "failed authentication must not durably bind an unowned legacy log to the rejected key"
        );
        let writer = ControlReceiptWriter::open(&log, &correct_key)
            .expect("the actual signer must still acquire ownership after a rejected startup");
        writer
            .mint(
                VerbObject::new("approval.reject.accepted.v1").unwrap(),
                Sha256Digest::of(b"healthy next control"),
                HolderId(HOLDER.into()),
                TokenId(uuid::Uuid::new_v4()),
                None,
                CostTuple::default(),
                NOW_MS,
            )
            .unwrap();
        let chain = load_persisted_chain(&log).unwrap();
        assert_eq!(chain.len(), 2);
        verify_persisted_chain_with_jwks(&chain, &Jwks::from_public_key(&correct_key.public_key()))
            .unwrap();
    }
}

#[test]
fn failed_runtime_authentication_does_not_bind_legacy_log() {
    failed_authentication_does_not_bind_legacy_log(true);
}

#[test]
fn failed_standalone_authentication_does_not_bind_legacy_log() {
    failed_authentication_does_not_bind_legacy_log(false);
}

#[test]
fn unterminated_whitespace_cannot_be_used_as_an_append_boundary() {
    let root = tempdir().unwrap();
    let log = root.path().join("chain.jsonl");
    let original = b" \t\r";
    std::fs::write(&log, original).unwrap();
    assert!(
        matches!(control(&log), Err(ReceiptChainError::Malformed(_))),
        "an append after unterminated whitespace would corrupt the next JWS line"
    );
    assert_eq!(std::fs::read(&log).unwrap(), original);
    // Explicit fixture repair, never automatic reader truncation.
    std::fs::write(&log, b"").unwrap();
    control(&log).unwrap();
    let chain = load_persisted_chain(&log).unwrap();
    assert_eq!(chain.len(), 1);
    verify_persisted_chain_with_jwks(&chain, &Jwks::from_public_key(&receipt_key().public_key()))
        .unwrap();
}
