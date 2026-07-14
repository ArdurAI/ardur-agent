//! Scenario §2.E1 — `cli_full_substrate_turn`.
//!
//! Drives one happy-path chat turn through the *fused* Phase-1 substrate and
//! asserts that every owning crate executed on a real call path — not as a
//! value-type import. The chain, in order:
//!
//! 1. **cap-token** — mint a root token, verify it against the issuer root.
//! 2. **cedar-policy** — the permissive policy authorizes the turn (`Allow`).
//! 3. **cost-gate** — reserve a generous envelope, finalize the actual cost, and
//!    prove the reservation is consumed (no leftover hold).
//! 4. **provider-runtime** — the Anthropic stub returns its deterministic output.
//! 5. **cli + runtime** — `ChatEngine::run_turn` fuses runtime + cost-gate +
//!    provider-registry through one async turn.
//! 6. **receipt** — mint the genesis receipt, verify its signature and the
//!    null `parent_hash` sentinel.
//! 7. **session-journals** — persist the turn to a file-backed JSONL journal and
//!    replay it off disk.
//! 8. **memory** — record the turn's fact and read it back as-of a later time.
//!
//! ## Adapt-points vs. the original §E1 plan
//!
//! The plan envisioned a single injectable `ChatRuntime` wired with all six
//! substrate components. No such constructor exists on `dev`: `cli::ChatEngine`
//! is the only orchestrator that fuses multiple crates on a real call path
//! (runtime + cost-gate + provider-registry), and it does *not* mint receipts,
//! write journals, or touch cap-token / cedar / memory. So this scenario uses
//! `ChatEngine::run_turn` as the fused spine and exercises the remaining crates
//! around it as a composed substrate proof. Each deviation is tagged
//! `// TODO §E1:` inline.

use ardur_e2e_tests::fixtures::{self, TEST_MODEL};

use std::sync::Arc;

use assert_matches::assert_matches;
use uuid::Uuid;

use ardur_cap_token::{
    BiscuitCapTokenVerifier, CapScope, CapTokenIssuer, CapTokenVerifier, HashSetDenyList,
    RequiredCaveats,
};
use ardur_cedar_policy::{
    ActionRef, Decision, EvaluationContext, PolicyBundle, PrincipalRef, ResourceRef,
};
use ardur_cli::{ChatEngine, Config};
use ardur_cost_gate::{
    AdmissionRequest, CostAdmissionGate, CostDelta, CostEnvelope, InMemoryBudgetStore,
    InMemoryCostAdmissionGate, ManualClock,
};
use ardur_memory::{InMemoryMemoryRuntime, MemoryRecord, MemoryRuntime, RecordKind};
use ardur_provider_runtime::{CompletionRequest, FinishReason, ModelId, Provider, Usage};
use ardur_receipt::{Jwks, ReceiptBody, ReceiptChain, ReceiptSigner, ReceiptVerifier, VerbObject};
use ardur_runtime::{ChatMessage, ReceiptId, SessionId};
use ardur_session_journals::{FileSessionJournal, JournalEntry, ReservationId, SessionJournal};

/// The SPIFFE-style principal the turn runs as.
const TEST_HOLDER: &str = "spiffe://ardur/user/e2e";
/// The audience the cap-token is scoped to.
const AUDIENCE: &str = "ardur-cli";
/// A fixed "now" (seconds) for the cap-token caveats — well before the token's
/// expiry so verification is time-stable.
const NOW_UNIX: u64 = 1_750_000_000;
/// The same instant in milliseconds, for the cost-gate clock and the journal /
/// receipt / memory timestamps.
const NOW_MS: u64 = 1_750_000_000_000;
/// The user prompt the turn submits.
const PROMPT: &str = "hello full substrate";

#[tokio::test]
async fn single_turn_through_full_substrate() {
    // ---- 1. cap-token: mint a root token and verify it against the root key.
    let issuer = fixtures::dev_cap_issuer();
    let root = issuer.public_key();
    let token = issuer
        .issue(
            ardur_cap_token::HolderId(TEST_HOLDER.to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix: NOW_UNIX + 3_600,
                budget_remaining: 10_000,
                // The plan's "chat.submit" + "memory.write" capability names —
                // the crate models these as opaque tool-allowlist strings.
                tool_allowlist: vec!["chat.submit".to_string(), "memory.write".to_string()],
            },
        )
        .expect("the root cap-token issues");

    let verifier = BiscuitCapTokenVerifier::new(HashSetDenyList::new());
    let claims = verifier
        .verify(
            &token,
            &root,
            &RequiredCaveats {
                now_unix: NOW_UNIX,
                audience: AUDIENCE.to_string(),
                tool: "chat.submit".to_string(),
                cost: 1,
            },
        )
        .expect("the freshly minted token verifies for chat.submit");
    assert_eq!(claims.audience, AUDIENCE);
    assert_eq!(
        claims.subject,
        ardur_cap_token::HolderId(TEST_HOLDER.to_string())
    );
    assert!(
        claims.tool_allowlist.iter().any(|t| t == "memory.write"),
        "the granted memory.write capability survived issuance"
    );

    // ---- 2. cedar-policy: the permissive bundle authorizes the turn.
    let policies = fixtures::permissive_policies();
    let decision = policies.evaluate(&EvaluationContext {
        principal: PrincipalRef("User::e2e".to_string()),
        action: ActionRef("Action::Submit".to_string()),
        resource: ResourceRef("Turn::t1".to_string()),
        attributes: serde_json::Value::Null,
    });
    assert_matches!(decision, Decision::Allow { matched_policy_ids } if !matched_policy_ids.is_empty());

    // ---- 3. cost-gate: reserve a generous envelope, then finalize the turn.
    let holder = ardur_cost_gate::HolderId(TEST_HOLDER.to_string());
    let budget = InMemoryBudgetStore::new();
    budget.set_balance(
        holder.clone(),
        ardur_cost_gate::CostTuple {
            tokens_in: 1_000_000,
            tokens_out: 1_000_000,
            cents: 10_000,
            wall_ms: 10_000_000,
            attention_score: 1_000_000,
        },
    );
    // A manual clock keeps reservation expiry deterministic (no wall-clock race).
    let gate = InMemoryCostAdmissionGate::with_clock(
        budget,
        // `ardur_cost_gate::UnixTsMillis` is a bare `u64` alias.
        Arc::new(ManualClock::new(NOW_MS)),
    );
    // Bind the cost-gate token id to the holder, reusing the verified cap-token's
    // id so the two crates name the same authority.
    let token_id = ardur_cost_gate::TokenId(claims.token_id);
    gate.bind_token(token_id, holder.clone());

    let envelope = CostEnvelope {
        tokens_in_max: 100_000,
        tokens_out_max: 100_000,
        cents_max: 10_000,
        wall_ms_max: 60_000,
        attention_score_max: 1_000,
    };
    let reservation = gate
        .admit(AdmissionRequest {
            cap_token_id: token_id,
            projected_envelope: envelope,
            provider_id: ardur_cost_gate::ProviderId("anthropic".to_string()),
            model_id: ardur_cost_gate::ModelId(TEST_MODEL.to_string()),
            request_digest: ardur_cost_gate::Sha256Digest::of(PROMPT.as_bytes()),
        })
        .await
        .expect("the turn is admitted against the budget");

    // ---- 4. provider-runtime: the stub returns its deterministic completion.
    let provider = fixtures::stub_provider();
    let response = provider
        .complete(CompletionRequest::new(
            vec![ChatMessage::user(PROMPT)],
            ModelId::new(TEST_MODEL),
            256,
        ))
        .await
        .expect("the stub provider completes");
    assert_eq!(response.content, "[anthropic stub]");
    assert_matches!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        response.usage,
        Usage {
            tokens_in: 0,
            tokens_out: 0,
            ..Default::default()
        }
    );

    // Finalize the reservation with the stub's actual (zero) usage and prove the
    // unspent hold is released in full.
    let reserved = ardur_cost_gate::CostTuple::from_envelope(&envelope);
    let actual = ardur_cost_gate::CostTuple::default();
    let refund = gate
        .finalize(reservation.clone(), actual)
        .await
        .expect("the reservation finalizes");
    assert_eq!(refund.actual, actual);
    assert_eq!(
        refund.refunded,
        CostDelta::between(&reserved, &actual),
        "the unspent reservation is credited back in full"
    );
    // No leftover reservation: the record was consumed, so a second finalize on
    // the same reservation is a no-such-reservation error.
    let second = gate.finalize(reservation, actual).await;
    assert_matches!(second, Err(ardur_cost_gate::AdmissionError::Internal(_)));

    // ---- 5. cli + runtime: the fused turn through ChatEngine.
    let config = Config {
        api_key: "test-key".to_string(),
        model: TEST_MODEL.to_string(),
        budget_cents: 10_000,
    };
    let engine = ChatEngine::new(&config).expect("the chat engine wires");
    let history = vec![
        ChatMessage::system("you are ardur"),
        ChatMessage::user(PROMPT),
    ];
    let outcome = engine
        .run_turn(&history)
        .await
        .expect("the turn completes through the fused substrate");
    // TODO §E1: the plan expected "the stub's deterministic output" here. In
    // Phase 1 `ChatEngine` routes turns through the `InMemoryRuntime` *echo*
    // stub rather than dispatching to the provider, so the deterministic output
    // on this path is the echoed prompt. The stub provider's own deterministic
    // output is asserted directly in stage 4 above.
    assert!(
        outcome.response.contains(PROMPT),
        "the runtime echoed the prompt deterministically"
    );
    // The stub bills nothing, so the turn reserves-then-finalizes to zero spend.
    assert_eq!(outcome.used_cents, 0);
    assert_eq!(outcome.remaining_cents, 10_000);

    // ---- 6. receipt: mint and verify the genesis receipt for this turn.
    let receipt_key = fixtures::dev_receipt_key();
    let jwks = Jwks::from_public_key(&receipt_key.public_key());
    let body = ReceiptChain::append(
        None,
        ReceiptBody {
            receipt_id: Uuid::new_v4(),
            parent_hash: None,
            verb: VerbObject::new("chat.completion.recorded.v1").expect("verb is well-formed"),
            issued_at: ardur_receipt::UnixTsMillis(NOW_MS),
            subject: ardur_receipt::HolderId(TEST_HOLDER.to_string()),
            cap_token_id: ardur_receipt::TokenId(claims.token_id),
            payload_digest: ardur_receipt::Sha256Digest::of(outcome.response.as_bytes()),
            session_id: None,
            cost: ardur_receipt::CostTuple {
                tokens_in: 0,
                tokens_out: 0,
                cents: 0,
                wall_ms: 0,
                attention_score: 0,
            },
            tool_calls: Vec::new(),
            provider: None,
        },
    );
    let signed = ReceiptSigner::sign(body, &receipt_key).expect("the genesis receipt signs");
    // The genesis receipt's parent_hash is the documented null sentinel — Option::None.
    assert!(
        signed.body().parent_hash.is_none(),
        "the genesis receipt has no parent"
    );
    // TODO §E1: the plan said "valid ed25519 sig". Receipts are signed with
    // ES256 (P-256 / ECDSA + SHA-256), not Ed25519 — the verifier proves the
    // signature is valid under the publishing JWKS.
    let verified = ReceiptVerifier::verify(&signed, &jwks).expect("the genesis receipt verifies");
    assert_eq!(verified.kid, receipt_key.key_id());
    ardur_receipt::verify_chain(std::slice::from_ref(&signed), &jwks)
        .expect("the single-receipt chain verifies");

    // ---- 7. session-journals: persist the turn and replay it off disk.
    let session_root = fixtures::temp_session_root();
    let session_id = SessionId::new();
    let journal = FileSessionJournal::new(session_root.path(), session_id)
        .expect("the file-backed journal opens");
    let receipt_id = ReceiptId::new();
    journal
        .append(JournalEntry::UserMessage {
            content: PROMPT.to_string(),
            at: NOW_MS,
        })
        .await
        .expect("the user message is journaled");
    journal
        .append(JournalEntry::AssistantMessage {
            content: outcome.response.clone(),
            at: NOW_MS + 1,
            receipt_id,
        })
        .await
        .expect("the assistant message is journaled");
    journal
        .append(JournalEntry::CostFinalized {
            reservation_id: ReservationId::from_uuid(refund.reservation_id),
            actual: refund.actual,
            refunded: refund.refunded,
            at: NOW_MS + 2,
        })
        .await
        .expect("the cost settlement is journaled");

    let replayed = journal
        .replay(session_id)
        .await
        .expect("the journal replays");
    assert!(
        !replayed.is_empty(),
        "the journal persisted at least one entry for the turn"
    );
    assert_matches!(replayed[0], JournalEntry::UserMessage { .. });

    // Parse the on-disk JSONL directly to prove durable persistence and the
    // `kind` discriminant the journal tags each line with.
    let raw = std::fs::read_to_string(journal.path()).expect("the journal file is readable");
    let lines: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty(), "the journal file has at least one line");
    let first: serde_json::Value =
        serde_json::from_str(lines[0]).expect("the first journal line is valid JSON");
    // TODO §E1: the plan guessed the submit `kind` would be "chat.submit" or
    // "turn.start". The journal tags entries by serde variant name, so a
    // submitted user message is tagged "UserMessage".
    assert_eq!(first["kind"], "UserMessage");

    // ---- 8. memory: record the turn's fact and read it back.
    // TODO §E1: the plan asked to assert the runtime *auto-persisted* the turn
    // into memory. `ChatEngine` has no `ardur-memory` dependency in Phase 1 and
    // does not auto-persist, so the memory leg of the substrate is exercised
    // directly here rather than as a side effect of `run_turn`.
    let memory = InMemoryMemoryRuntime::new();
    let subject = ardur_memory::HolderId(TEST_HOLDER.to_string());
    memory
        .record(MemoryRecord::new(
            subject.clone(),
            RecordKind::Fact,
            serde_json::json!({ "turn_response": outcome.response }),
            ardur_memory::UnixTsMillis(NOW_MS),
            ardur_memory::UnixTsMillis(NOW_MS),
            None,
            ardur_memory::UnixTsMillis(NOW_MS),
        ))
        .expect("the turn fact is recorded");
    let visible = memory.current_as_of(&subject, ardur_memory::UnixTsMillis(NOW_MS + 1));
    assert_eq!(
        visible.len(),
        1,
        "the recorded fact is visible as-of a later transaction time"
    );

    // Hold the temp root until the end so the journal file outlives its reads.
    drop(session_root);
}
