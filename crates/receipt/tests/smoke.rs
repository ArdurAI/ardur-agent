//! Behavioral tests for §11.14 Phase 1: sign/verify, hash-chaining, verb
//! grammar, key custody, and the JWKS round-trip. Every public error variant
//! the verifier distinguishes is exercised.

use ardur_receipt::{
    CostTuple, Es256SigningKey, HolderId, Jwks, ReceiptBody, ReceiptChain, ReceiptError,
    ReceiptSigner, ReceiptVerifier, Sha256Digest, TokenId, UnixTsMillis, VerbObject,
};

fn sample_body(verb: &str) -> ReceiptBody {
    ReceiptBody {
        receipt_id: uuid::Uuid::new_v4(),
        parent_hash: None,
        verb: VerbObject::new(verb).expect("test verb is well-formed"),
        issued_at: UnixTsMillis(1_700_000_000_000),
        subject: HolderId("spiffe://ardur/user/alice".to_string()),
        cap_token_id: TokenId("jti-0001".to_string()),
        payload_digest: Sha256Digest::of(b"event-payload"),
        session_id: None,
        cost: CostTuple {
            tokens_in: 100,
            tokens_out: 50,
            cents: 2,
            wall_ms: 1_200,
            attention_score: 500,
        },
        tool_calls: Vec::new(),
        provider: None,
    }
}

#[test]
fn sign_verify_roundtrip() {
    let key = Es256SigningKey::generate();
    let jwks = Jwks::from_public_key(&key.public_key());

    let body = sample_body("cost.admission.allow.v1");
    let signed = ReceiptSigner::sign(body.clone(), &key).unwrap();
    let verified = ReceiptVerifier::verify(&signed, &jwks).unwrap();

    assert_eq!(verified.body, body);
    assert_eq!(verified.kid, key.key_id());
}

#[test]
fn chain_3_receipts() {
    let key = Es256SigningKey::generate();
    let jwks = Jwks::from_public_key(&key.public_key());

    let r0 = ReceiptSigner::sign(
        ReceiptChain::append(None, sample_body("cap.issued.allow.v1")),
        &key,
    )
    .unwrap();
    let r1 = ReceiptSigner::sign(
        ReceiptChain::append(Some(&r0), sample_body("cost.admission.allow.v1")),
        &key,
    )
    .unwrap();
    let r2 = ReceiptSigner::sign(
        ReceiptChain::append(Some(&r1), sample_body("cost.finalize.recorded.v1")),
        &key,
    )
    .unwrap();

    let chain = vec![r0, r1, r2];
    ardur_receipt::verify_chain(&chain, &jwks).expect("freshly built chain verifies");

    assert!(
        chain[0].body().parent_hash.is_none(),
        "genesis has no parent"
    );
    assert_eq!(
        chain[1].body().parent_hash,
        Some(Sha256Digest::of(chain[0].jws_compact().as_bytes()))
    );
    for receipt in &chain {
        ReceiptVerifier::verify(receipt, &jwks).expect("each receipt verifies under the JWKS");
    }
}

#[test]
fn chain_tampered() {
    let key = Es256SigningKey::generate();

    let r0 = ReceiptSigner::sign(
        ReceiptChain::append(None, sample_body("cap.issued.allow.v1")),
        &key,
    )
    .unwrap();
    let r1 = ReceiptSigner::sign(
        ReceiptChain::append(Some(&r0), sample_body("cost.admission.allow.v1")),
        &key,
    )
    .unwrap();

    // Swap the genesis for a different (validly signed) one. r1 still points
    // at the original r0's hash, so the link breaks at index 1.
    let alt_genesis = ReceiptSigner::sign(
        ReceiptChain::append(None, sample_body("cap.issued.deny.v1")),
        &key,
    )
    .unwrap();

    let tampered = vec![alt_genesis, r1];
    match ardur_receipt::verify_chain(&tampered, &Jwks::from_public_key(&key.public_key())) {
        Err(ReceiptError::BrokenChain(broken)) => {
            assert_eq!(broken.at, 1);
            assert_ne!(broken.expected, broken.actual);
        }
        other => panic!("expected BrokenChain at index 1, got {other:?}"),
    }
}

#[test]
fn chain_rejects_forged_signature() {
    // ARD-479: a receipt whose parent_hash links correctly but whose JWS was
    // signed by a stranger key must NOT pass verify_chain. Its kid is absent
    // from the JWKS, so the per-receipt signature check now rejects it.
    let key = Es256SigningKey::generate();
    let jwks = Jwks::from_public_key(&key.public_key());

    let r0 = ReceiptSigner::sign(
        ReceiptChain::append(None, sample_body("cap.issued.allow.v1")),
        &key,
    )
    .unwrap();
    // Same body (so parent_hash still links to r0), signed by a stranger.
    let stranger = Es256SigningKey::generate();
    let forged_r1 = ReceiptSigner::sign(
        ReceiptChain::append(Some(&r0), sample_body("cost.admission.allow.v1")),
        &stranger,
    )
    .unwrap();

    let chain = vec![r0, forged_r1];
    match ardur_receipt::verify_chain(&chain, &jwks) {
        Err(ReceiptError::UnknownKid(_)) => (),
        other => panic!("expected UnknownKid from forged signature, got {other:?}"),
    }
}

#[test]
fn verb_format() {
    // Valid: four segments, integer version (single and multi-digit).
    assert!(VerbObject::new("cost.admission.allow.v1").is_ok());
    assert!(VerbObject::new("tool.call.completed.v12").is_ok());
    // Invalid: uppercase segment.
    assert!(matches!(
        VerbObject::new("Cost.admission.allow.v1"),
        Err(ReceiptError::InvalidVerb(_))
    ));
    // Invalid: only three segments (missing `state`).
    assert!(matches!(
        VerbObject::new("cost.admission.v1"),
        Err(ReceiptError::InvalidVerb(_))
    ));
    // Invalid: version lacks the `v` prefix.
    assert!(matches!(
        VerbObject::new("cost.admission.allow.1"),
        Err(ReceiptError::InvalidVerb(_))
    ));
}

#[test]
fn unknown_kid() {
    let signer = Es256SigningKey::generate();
    let stranger = Es256SigningKey::generate();
    // JWKS holds only the stranger's key, so the signer's kid is absent.
    let jwks = Jwks::from_public_key(&stranger.public_key());

    let signed = ReceiptSigner::sign(sample_body("cost.admission.allow.v1"), &signer).unwrap();
    match ReceiptVerifier::verify(&signed, &jwks) {
        Err(ReceiptError::UnknownKid(kid)) => assert_eq!(kid, signer.key_id()),
        other => panic!("expected UnknownKid, got {other:?}"),
    }
}

#[test]
fn signature_invalid_on_key_mismatch() {
    let signer = Es256SigningKey::generate();
    let impostor = Es256SigningKey::generate();
    // Same kid the signer advertises, but the wrong key material behind it.
    let mut jwk = impostor.public_key().to_jwk();
    jwk.kid = signer.key_id();
    let jwks = Jwks(vec![jwk]);

    let signed = ReceiptSigner::sign(sample_body("cost.admission.allow.v1"), &signer).unwrap();
    assert!(matches!(
        ReceiptVerifier::verify(&signed, &jwks),
        Err(ReceiptError::SignatureInvalid)
    ));
}

#[test]
fn pkcs8_pem_roundtrip() {
    let key = Es256SigningKey::generate();
    let pem = key.to_pkcs8_pem().unwrap();
    let restored = Es256SigningKey::from_pkcs8_pem(&pem).unwrap();

    assert_eq!(key.key_id(), restored.key_id());
    let jwks = Jwks::from_public_key(&key.public_key());
    let signed = ReceiptSigner::sign(sample_body("cost.admission.allow.v1"), &restored).unwrap();
    ReceiptVerifier::verify(&signed, &jwks).expect("reloaded key signs verifiably");
}

#[test]
fn jwks_json_roundtrip() {
    let key = Es256SigningKey::generate();
    let jwks = Jwks::from_public_key(&key.public_key());

    let json = serde_json::to_string(&jwks).unwrap();
    let parsed: Jwks = serde_json::from_str(&json).unwrap();
    assert_eq!(jwks, parsed);

    let signed = ReceiptSigner::sign(sample_body("cost.admission.allow.v1"), &key).unwrap();
    ReceiptVerifier::verify(&signed, &parsed).expect("verifies against round-tripped JWKS");
}

/// §11.14b backward-compat: a receipt body serialized before the `provider`
/// field existed (no `"provider"` key) decodes with `provider == None`.
#[test]
fn provider_field_backward_compat_decode() {
    let pre = sample_body("llm.completion.minted.v1");
    let mut json = serde_json::to_value(&pre).expect("body serializes");
    // Strip the field to model a pre-§11.14b receipt on disk.
    json.as_object_mut().unwrap().remove("provider");
    assert!(
        !json.as_object().unwrap().contains_key("provider"),
        "the pre-§11.14b fixture has no provider key"
    );

    let decoded: ReceiptBody = serde_json::from_value(json).expect("legacy body decodes");
    assert_eq!(decoded.provider, None, "absent provider key loads as None");
    assert_eq!(
        decoded, pre,
        "round-trips to the original None-provider body"
    );
}

/// §11.14b forward-compat: a `None`-provider body serializes byte-identically to
/// a pre-§11.14b one — `skip_serializing_if` omits the key, so the signature and
/// chain hash of a no-provider receipt are unchanged.
#[test]
fn provider_none_is_byte_identical() {
    let none = sample_body("llm.completion.minted.v1");
    assert_eq!(none.provider, None);

    let bytes = serde_json::to_vec(&none).expect("body serializes");
    let text = String::from_utf8(bytes).unwrap();
    assert!(
        !text.contains("\"provider\""),
        "a None provider must not emit a provider key: {text}"
    );

    // A populated provider DOES emit the key — the omission above is the
    // None case specifically, not the field being dropped outright.
    let mut some = none.clone();
    some.provider = Some("anthropic".to_string());
    let some_text = serde_json::to_string(&some).expect("body serializes");
    assert!(some_text.contains("\"provider\":\"anthropic\""));
}

/// §11.14b: a populated `provider` survives a sign → verify round-trip — it is
/// inside the signed body, not metadata bolted on after minting.
#[test]
fn provider_field_survives_sign_verify() {
    let key = Es256SigningKey::generate();
    let jwks = Jwks::from_public_key(&key.public_key());

    let mut body = sample_body("llm.completion.minted.v1");
    body.provider = Some("anthropic".to_string());

    let signed = ReceiptSigner::sign(body.clone(), &key).unwrap();
    let verified = ReceiptVerifier::verify(&signed, &jwks).unwrap();
    assert_eq!(verified.body.provider.as_deref(), Some("anthropic"));
    assert_eq!(verified.body, body);
}
