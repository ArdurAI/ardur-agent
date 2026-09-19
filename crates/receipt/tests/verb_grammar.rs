//! INTER-01 / #537 guard: the receipt verb grammar is `verb.object.state.vN`
//! (exactly four dot-separated segments), and every verb the plan corpus
//! promises to mint must be representable under it — by RESPELLING, not by
//! silently widening the wire format.
//!
//! The four planned verbs that violated the grammar (`plane.unreachable.v1`,
//! `setup.completed.v1`, `auth.login.v1`, `auth.revoked.v1`) are pinned here in
//! BOTH forms: the respelled spelling must parse and pipeline end-to-end, and
//! the original three-segment spelling must stay rejected so a future grammar
//! widening cannot land silently (such a widening is a wire-format change to
//! every receipt consumer and requires an explicit design note).
//!
//! Plan references (architect/plans/, local plan corpus):
//! - `plane.unreachable.v1` — 2026-09-17-ardur-convergence-contract-map.md §9 B5
//! - `setup.completed.v1`  — 2026-09-18-ux-stack-plan.md §3.1 step 5
//! - `auth.login.v1` / `auth.revoked.v1` — 2026-09-18-ux-stack-plan.md §3.2
//! - `governance.dualrun.mismatch.v1` — convergence map §9 B4 (already valid)
//! - `llm.completion.minted.v1` — 2026-09-17-ardur-beta-runbook.md (already valid)

use ardur_receipt::{
    CostTuple, HolderId, ReceiptBody, ReceiptChain, ReceiptSigner, ReceiptVerifier, Sha256Digest,
    TokenId, UnixTsMillis, VerbObject,
};

/// Every verb named in the plan corpus, its disposition, and where it came
/// from. The table is the executable contract: if the grammar or a plan
/// changes, this table must change in the same PR, deliberately.
const VERB_TABLE: &[(&str, bool, &str)] = &[
    // --- Respelled planned verbs (the #537 fix): must parse. ---
    (
        "governance.plane.unreachable.v1",
        true,
        "B5 plane-outage marker, respelled from plane.unreachable.v1",
    ),
    (
        "setup.wizard.completed.v1",
        true,
        "UX §3.1 step 5 first-receipt, respelled from setup.completed.v1",
    ),
    (
        "auth.login.succeeded.v1",
        true,
        "UX §3.2 /login receipt, respelled from auth.login.v1",
    ),
    (
        "auth.credential.revoked.v1",
        true,
        "UX §3.2 /logout receipt, respelled from auth.revoked.v1",
    ),
    // --- Already-valid planned verbs: must keep parsing (controls). ---
    (
        "governance.dualrun.mismatch.v1",
        true,
        "convergence §9 B4 shadow-mode mismatch evidence",
    ),
    (
        "llm.completion.minted.v1",
        true,
        "beta runbook completion receipt",
    ),
    // --- Grammar controls. ---
    (
        "cost.admission.allow.v1",
        true,
        "four-segment control: proves this test cannot pass by denying everything",
    ),
    (
        "llm.completion.cancelled.v10",
        true,
        "two-digit version control: v[0-9]+ is not limited to v1",
    ),
    // --- Original three-segment plan spellings: must stay REJECTED. ---
    (
        "plane.unreachable.v1",
        false,
        "original B5 spelling; rejection pins the grammar against silent widening",
    ),
    ("setup.completed.v1", false, "original UX §3.1 spelling"),
    ("auth.login.v1", false, "original UX §3.2 spelling"),
    ("auth.revoked.v1", false, "original UX §3.2 spelling"),
    // --- Grammar edge controls: must be rejected. ---
    ("", false, "empty"),
    ("not-a-verb", false, "deny-all control"),
    ("Cost.Admission.Allow.v1", false, "uppercase segments"),
    ("a.b.c.d.v1", false, "five segments"),
    (
        "cost.admission.allow.1",
        false,
        "missing v prefix on version",
    ),
    ("cost.admission.allow.V1", false, "uppercase V"),
];

#[test]
fn the_plan_corpus_verb_table_matches_the_native_grammar() {
    assert!(
        VERB_TABLE.len() >= 16,
        "the table must cover every planned verb plus controls"
    );
    let mut parsed = 0usize;
    let mut rejected = 0usize;
    for (verb, expected, origin) in VERB_TABLE {
        let result = VerbObject::new(*verb);
        assert_eq!(
            result.is_ok(),
            *expected,
            "verb `{verb}` ({origin}): expected parse={expected}, got {result:?}"
        );
        if *expected {
            parsed += 1;
        } else {
            rejected += 1;
        }
    }
    // Population pins: a vacuous table (all-parse or all-reject) is a bug in
    // the guard itself.
    assert_eq!(parsed, 8, "eight verbs must parse");
    assert_eq!(rejected, VERB_TABLE.len() - 8, "the rest must reject");
}

fn body_with(verb: &str, cost: CostTuple) -> ReceiptBody {
    ReceiptBody {
        receipt_id: uuid::Uuid::new_v4(),
        parent_hash: None,
        verb: VerbObject::new(verb).expect("table verb is well-formed"),
        issued_at: UnixTsMillis(1_700_000_000_000),
        subject: HolderId("spiffe://ardur/user/alice".to_string()),
        cap_token_id: TokenId(uuid::Uuid::from_u128(0x0001)),
        payload_digest: Sha256Digest::of(b"verb-guard-payload"),
        session_id: None,
        cost,
        tool_calls: Vec::new(),
        provider: None,
    }
}

const ZERO_COST: CostTuple = CostTuple {
    tokens_in: 0,
    tokens_out: 0,
    cents: 0,
    wall_ms: 0,
    attention_score: 0,
};

/// The respelled verbs must pipeline end-to-end: construction, ES256 signing,
/// chain append, and chain verification. The outage/mismatch markers are
/// exercised carrying a zero cost tuple — they are cost-neutral markers
/// (#537 guard requirement 4), so the receipt layer must accept them without
/// any verb-keyed special casing.
#[test]
fn the_respelled_verbs_sign_and_chain_end_to_end() {
    let key = ardur_receipt::Es256SigningKey::generate();
    let jwks = ardur_receipt::Jwks::from_public_key(&key.public_key());

    let respelled = [
        "governance.plane.unreachable.v1",
        "setup.wizard.completed.v1",
        "auth.login.succeeded.v1",
        "auth.credential.revoked.v1",
        // The already-merged dualrun mismatch marker chains the same way.
        "governance.dualrun.mismatch.v1",
    ];

    let mut chain = Vec::new();
    for verb in respelled {
        // Markers carry zero cost; login/setup receipts here use zero too —
        // the debit they describe is receipted on its own completion verb.
        let body = body_with(verb, ZERO_COST);
        let signed = ReceiptSigner::sign(ReceiptChain::append(chain.last(), body), &key)
            .unwrap_or_else(|e| panic!("verb `{verb}` must sign: {e}"));
        chain.push(signed);
    }
    ardur_receipt::verify_chain(&chain, &jwks).expect("the respelled-verb chain verifies");
    for (i, receipt) in chain.iter().enumerate() {
        ReceiptVerifier::verify(receipt, &jwks)
            .unwrap_or_else(|e| panic!("chain link {i} must verify: {e}"));
        assert_eq!(receipt.body().cost, ZERO_COST);
    }
    assert_eq!(chain.len(), 5);
}

/// Deserialization routes through the validating constructor, so a stored or
/// received receipt carrying an original three-segment spelling must fail to
/// deserialize — the invariant holds for the wire path, not just `new`.
#[test]
fn the_original_spellings_are_rejected_on_deserialize() {
    for verb in [
        "plane.unreachable.v1",
        "setup.completed.v1",
        "auth.login.v1",
        "auth.revoked.v1",
    ] {
        let json = serde_json::Value::String(verb.to_string());
        assert!(
            serde_json::from_value::<VerbObject>(json).is_err(),
            "`{verb}` must not deserialize"
        );
    }
    let ok = serde_json::from_value::<VerbObject>(serde_json::Value::String(
        "governance.plane.unreachable.v1".to_string(),
    ));
    assert!(ok.is_ok(), "the respelled spelling deserializes");
}
