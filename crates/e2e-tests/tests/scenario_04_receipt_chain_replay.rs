//! Scenario §2.E4 — `receipt_chain_replay`.
//!
//! Proves the receipt hash-chain and the session journal survive a process
//! restart: a *fresh* fused runtime, pointed at the same on-disk paths, resumes
//! the chain rather than starting a new genesis.
//!
//! 1. Runtime **A** submits three turns, persisting three signed receipts (one
//!    compact JWS per line) and journaling each turn.
//! 2. A is dropped — the in-memory chain tail is gone; only the files remain.
//! 3. Runtime **B** is built over the same receipt-log + journal paths (same
//!    cap-token root and receipt key). It seeds its chain tail from the last
//!    persisted receipt and submits a fourth turn.
//! 4. The fourth receipt — minted by the *restarted* runtime — chains onto the
//!    third receipt's JWS, and [`verify_persisted_chain`] passes over all four.
//!    The journal, reopened off disk, replays every turn.
//!
//! This is the cross-restart linkage E2E #4 in
//! `architect/backlog/e2e-test-coverage-gaps.md`.

use std::sync::Arc;

use ardur_e2e_tests::fixtures::{self};

use ardur_fused_runtime::{load_persisted_chain, verify_persisted_chain};
use ardur_receipt::Sha256Digest;
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};
use ardur_session_journals::{FileSessionJournal, SessionJournal};

mod support;
use support::EchoProvider;

#[tokio::test]
async fn receipt_chain_and_journal_survive_a_restart() {
    let root = fixtures::temp_session_root();
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = SessionId::new();
    let token = fixtures::dev_valid_cap_token();

    let request = |content: &str| SubmitRequest {
        messages: vec![ChatMessage::user(content)],
        cap_token: CapTokenRef(token.clone()),
        session_id,
        requested_provider: None,
    };

    // ---- 1. Runtime A: three turns, persisted to the same paths.
    {
        let provider = Arc::new(EchoProvider::new());
        let journal =
            Arc::new(FileSessionJournal::new(root.path(), session_id).expect("journal A opens"));
        let runtime_a = fixtures::fused_builder(provider.clone())
            .with_journal(journal.clone())
            .receipt_log(&receipt_log)
            .build()
            .expect("runtime A wires");

        for prompt in ["turn one", "turn two", "turn three"] {
            runtime_a
                .submit(request(prompt))
                .await
                .expect("each pre-restart turn completes");
        }
        assert_eq!(provider.call_count(), 3);

        // Three receipts persisted, properly chained.
        let chain = load_persisted_chain(&receipt_log).expect("chain loads");
        assert_eq!(chain.len(), 3);
        verify_persisted_chain(&chain).expect("the pre-restart chain verifies");

        // The journal holds two entries per turn.
        let replayed = journal.replay(session_id).await.expect("journal A replays");
        assert_eq!(replayed.len(), 6, "three turns × (user + assistant)");

        // ---- 2. A is dropped here at end of scope — only the files remain.
    }

    // ---- 3. Runtime B over the SAME paths resumes the chain.
    let provider_b = Arc::new(EchoProvider::new());
    let journal_b =
        Arc::new(FileSessionJournal::new(root.path(), session_id).expect("journal B reopens"));
    let runtime_b = fixtures::fused_builder(provider_b.clone())
        .with_journal(journal_b.clone())
        .receipt_log(&receipt_log)
        .build()
        .expect("runtime B wires over the persisted paths");

    runtime_b
        .submit(request("turn four"))
        .await
        .expect("the post-restart turn completes");
    assert_eq!(provider_b.call_count(), 1, "B dispatched the fourth turn");

    // ---- 4. The fourth receipt chains onto the third — across the restart.
    let chain = load_persisted_chain(&receipt_log).expect("chain reloads");
    assert_eq!(
        chain.len(),
        4,
        "the restart appended, not restarted, the chain"
    );
    assert!(chain[0].body.parent_hash.is_none(), "genesis unchanged");
    assert_eq!(
        chain[3].body.parent_hash,
        Some(Sha256Digest::of(chain[2].jws_compact.as_bytes())),
        "the restarted runtime's receipt chains onto the pre-restart tail"
    );
    verify_persisted_chain(&chain).expect("the full chain verifies across the restart");

    // The journal, reopened off disk, replays all four turns.
    let replayed = journal_b
        .replay(session_id)
        .await
        .expect("journal B replays");
    assert_eq!(
        replayed.len(),
        8,
        "four turns × (user + assistant), durable across restart"
    );

    drop(root);
}
