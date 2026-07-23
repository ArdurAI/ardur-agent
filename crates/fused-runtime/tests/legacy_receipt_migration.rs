//! Regression for issue #350: a receipt persisted by an older build carries
//! `CostTuple.attention_score` as a `0.0..=1.0` float, but the field is now a
//! milli-attention `u64`. Before the migration-tolerant deserializer, that
//! single line failed `serde_json` decode during boot-time chain reconciliation
//! (`invalid type: floating point, expected u64`) and aborted CLI + server
//! startup for that data dir, with no migration and no recovery path.
//!
//! These tests reproduce the exact on-disk shape — a genuinely ES256-signed
//! compact JWS whose payload's `attention_score` is a float — and assert that
//! the real boot path now loads it, migrates the value onto the milli-attention
//! axis, and still authenticates the signature over the original bytes (the
//! trust checks are preserved; only the decoded representation is coerced).

use std::sync::Arc;

use ardur_fused_runtime::{load_persisted_chain, verify_persisted_chain_with_jwks};
use ardur_receipt::{
    CostTuple, Es256SigningKey, HolderId, Jwks, ReceiptBody, Sha256Digest, TokenId, UnixTsMillis,
    VerbObject,
};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature as P256Signature, SigningKey as P256SigningKey};
use p256::pkcs8::DecodePrivateKey as _;

mod support;
use support::{EchoProvider, receipt_key, runtime_builder};

/// A genesis receipt body (`parent_hash == None`) shaped like the ones the
/// fused runtime mints for a completion turn.
fn genesis_body() -> ReceiptBody {
    ReceiptBody {
        receipt_id: uuid::Uuid::new_v4(),
        parent_hash: None,
        verb: VerbObject::new("llm.completion.minted.v1").expect("valid verb"),
        issued_at: UnixTsMillis(1_700_000_000_000),
        subject: HolderId("cli://localhost".to_string()),
        cap_token_id: TokenId(uuid::Uuid::from_u128(0x350)),
        payload_digest: Sha256Digest::of(b"payload"),
        session_id: None,
        cost: CostTuple::ZERO,
        tool_calls: Vec::new(),
        provider: Some("anthropic".to_string()),
    }
}

/// Sign `body` into a compact JWS the way an *older* build did — with
/// `cost.attention_score` serialized as the legacy `0.0..=1.0` float rather than
/// the milli-attention integer the current signer emits. We re-import the
/// receipt signing key through its PKCS#8 PEM (its raw signer is crate-private)
/// so the signature is produced by the very key the runtime's JWKS trusts, over
/// exactly the hand-built legacy bytes.
fn legacy_signed_jws(key: &Es256SigningKey, body: &ReceiptBody, attention_share: f64) -> String {
    let pem = key.to_pkcs8_pem().expect("export receipt key");
    let signing_key = P256SigningKey::from_pkcs8_pem(&pem).expect("re-import receipt key");

    // Header identical to `ReceiptSigner::sign`'s (field order is irrelevant —
    // the signature covers the stored base64 segments, not a re-serialization).
    let header = serde_json::json!({
        "alg": "ES256",
        "kid": key.key_id(),
        "typ": "ardur-receipt+jws",
    });
    // Serialize the body, then overwrite the cost's attention axis with a JSON
    // float — `serde_json` renders `0.5` as `0.5`, reproducing the legacy shape.
    let mut payload = serde_json::to_value(body).expect("body to json");
    payload["cost"]["attention_score"] = serde_json::json!(attention_share);

    let header_b64 = B64URL.encode(serde_json::to_vec(&header).expect("header bytes"));
    let payload_b64 = B64URL.encode(serde_json::to_vec(&payload).expect("payload bytes"));
    let signing_input = format!("{header_b64}.{payload_b64}");

    let sig: P256Signature = signing_key.sign(signing_input.as_bytes());
    // Match the signer/verifier's canonical low-S requirement (ARD-483).
    let sig = sig.normalize_s().unwrap_or(sig);
    format!("{signing_input}.{}", B64URL.encode(sig.to_bytes()))
}

/// The reported failure: booting the fused runtime over a data dir whose receipt
/// log holds a single legacy float-attention receipt must succeed, not abort.
#[tokio::test]
async fn boot_tolerates_legacy_float_attention_receipt() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_dir = root.path().join("receipts");
    std::fs::create_dir_all(&receipt_dir).expect("receipts dir");
    let receipt_log = receipt_dir.join("chain.jsonl");

    let key = receipt_key();
    // 0.5 legacy share → 500 milli-attention (lossless, per MILLI_ATTENTION_PER_UNIT).
    let jws = legacy_signed_jws(&key, &genesis_body(), 0.5);
    std::fs::write(&receipt_log, format!("{jws}\n")).expect("seed legacy receipt log");

    // The real boot path: `.build()` runs `load_persisted_chain` +
    // `verify_persisted_chain_with_jwks` over the seeded log. Before the fix this
    // returned `ReceiptChainError::Malformed("payload json: invalid type:
    // floating point ...")` and bricked startup.
    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime_builder(provider)
        .receipt_log(&receipt_log)
        .build()
        .expect("boot must tolerate a legacy float-attention receipt");
    drop(runtime);

    // The legacy share was migrated onto the milli-attention axis on load.
    let chain = load_persisted_chain(&receipt_log).expect("chain loads");
    assert_eq!(chain.len(), 1, "the one legacy receipt is loaded");
    assert!(
        chain[0].body.parent_hash.is_none(),
        "it is a genesis receipt"
    );
    assert_eq!(
        chain[0].body.cost.attention_score, 500,
        "legacy 0.5 share must migrate to 500 milli-attention"
    );

    // Integrity is preserved: the signature still authenticates over the
    // original on-disk bytes, and the migrated body matches the verified body.
    let jwks = Jwks::from_public_key(&key.public_key());
    verify_persisted_chain_with_jwks(&chain, &jwks)
        .expect("legacy receipt still authenticates after migration");
}

/// A legacy `0.0` share (the exact value in the audit's reproduction receipt)
/// migrates to `0` and loads cleanly.
#[tokio::test]
async fn legacy_zero_float_attention_loads() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_dir = root.path().join("receipts");
    std::fs::create_dir_all(&receipt_dir).expect("receipts dir");
    let receipt_log = receipt_dir.join("chain.jsonl");

    let key = receipt_key();
    let jws = legacy_signed_jws(&key, &genesis_body(), 0.0);
    std::fs::write(&receipt_log, format!("{jws}\n")).expect("seed legacy receipt log");

    let chain = load_persisted_chain(&receipt_log).expect("chain loads");
    assert_eq!(chain[0].body.cost.attention_score, 0);
    let jwks = Jwks::from_public_key(&key.public_key());
    verify_persisted_chain_with_jwks(&chain, &jwks).expect("authenticates");
}
