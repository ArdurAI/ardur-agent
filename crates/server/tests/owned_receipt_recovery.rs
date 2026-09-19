//! The real server journal owner is stable and distinct from request sessions.
mod support;
use ardur_fused_runtime::{load_persisted_chain, verify_persisted_chain_with_jwks};
use ardur_runtime::SessionId;
use ardur_session_journals::JournalEntry;

#[tokio::test]
async fn appstate_restart_recovers_all_owned_missing_answers_once() {
    let dir = tempfile::tempdir().unwrap();
    let config = support::test_config_http_only(&dir);
    let first = support::boot_stub(&config).await;
    let owner = *first.journal().session_id();
    let mut ids = Vec::new();
    for message in ["first", "second", "healthy"] {
        let request_session = SessionId::new();
        assert_ne!(request_session, owner);
        let reply = first
            .submit_chat(message.into(), request_session)
            .await
            .unwrap();
        ids.push(reply.receipt_id);
    }
    let log = config.data_dir.join("receipts/chain.jsonl");
    let receipt_bytes = std::fs::read(&log).unwrap();
    let path = config
        .data_dir
        .join("journals/sessions")
        .join(owner.0.to_string())
        .join("journal.jsonl");
    first.finish_shutdown().await.unwrap();
    drop(first);
    // Remove only the two derived answers; keep signed receipts AND ownership snapshots.
    let original = std::fs::read_to_string(&path).unwrap();
    let retained = original
        .lines()
        .filter(|line| {
            !(line.contains("AssistantMessage") && ids[..2].iter().any(|id| line.contains(id)))
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_ne!(original, retained);
    std::fs::write(&path, retained).unwrap();
    let second = support::boot_stub(&config).await;
    assert_eq!(*second.journal().session_id(), owner);
    let entries = second.journal().replay(owner).await.unwrap();
    for id in &ids {
        let answers: Vec<_> = entries
            .iter()
            .filter_map(|entry| match entry {
                JournalEntry::AssistantMessage {
                    receipt_id,
                    content,
                    ..
                } if receipt_id.0.to_string() == *id => Some(content),
                _ => None,
            })
            .collect();
        assert_eq!(
            answers.len(),
            1,
            "each owned committed answer must be recovered exactly once"
        );
        if ids[..2].contains(id) {
            assert!(answers[0].starts_with("[reconciled]"));
        } else {
            assert!(!answers[0].starts_with("[reconciled]"));
        }
    }
    let chain = load_persisted_chain(&log).unwrap();
    let key = ardur_receipt::Es256SigningKey::from_pkcs8_pem(
        &std::fs::read_to_string(config.data_dir.join("keys/receipt.pem")).unwrap(),
    )
    .unwrap();
    verify_persisted_chain_with_jwks(
        &chain,
        &ardur_receipt::Jwks::from_public_key(&key.public_key()),
    )
    .unwrap();
    assert_eq!(std::fs::read(&log).unwrap(), receipt_bytes);
    second.finish_shutdown().await.unwrap();
    drop(second);
    let recovered = std::fs::read(&path).unwrap();
    let third = support::boot_stub(&config).await;
    assert_eq!(
        std::fs::read(&path).unwrap(),
        recovered,
        "repeated real boot must not append"
    );
    assert!(
        third
            .settlement_supervisor()
            .status()
            .boot_problem
            .is_none()
    );
    third.finish_shutdown().await.unwrap();
}
