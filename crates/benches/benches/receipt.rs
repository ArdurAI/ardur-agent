//! Benchmarks for `ardur-receipt` — JWS-ES256 signing/verification and the
//! tamper-evident hash chain.
//!
//! Every metered turn mints and signs at least one receipt, and every
//! reconciliation / audit sweep verifies a chain of them, so both the sign and
//! verify paths are on the request/audit hot path. The cost is dominated by the
//! P-256 ECDSA primitive (a scalar multiply on sign, two on verify) plus the
//! serde_json + base64url framing; `verify_chain` additionally recomputes a
//! SHA-256 per link.
//!
//! These benchmarks measure behaviour that is security-critical, so the point is
//! a *baseline to guard against regression* — not a target to shave. Any
//! optimisation here must not weaken the low-S malleability check or the
//! verify-before-trust ordering in `verify_chain`.

use std::hint::black_box;

use ardur_receipt::{
    CostTuple, Es256SigningKey, HolderId, Jwks, ReceiptBody, ReceiptChain, ReceiptSigner,
    ReceiptVerifier, Sha256Digest, SignedReceipt, TokenId, UnixTsMillis, VerbObject, verify_chain,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

/// A representative receipt body (a cost-admission-allow receipt with a single
/// turn's cost, no tool calls) — the common shape minted per turn.
fn sample_body(seq: u64) -> ReceiptBody {
    ReceiptBody {
        receipt_id: uuid::Uuid::new_v4(),
        parent_hash: None,
        verb: VerbObject::new("cost.admission.allow.v1").unwrap(),
        issued_at: UnixTsMillis(1_700_000_000_000 + seq),
        subject: HolderId("spiffe://ardur/user/alice".to_string()),
        cap_token_id: TokenId("jti-0001".to_string()),
        payload_digest: Sha256Digest::of(b"event-payload"),
        session_id: None,
        cost: CostTuple {
            tokens_in: 100,
            tokens_out: 50,
            cents: 2,
            wall_ms: 1_200,
            attention_score: 0.5,
        },
        tool_calls: Vec::new(),
        provider: None,
    }
}

fn bench_sign(c: &mut Criterion) {
    let key = Es256SigningKey::generate();
    c.bench_function("receipt/sign", |b| {
        // Fresh body per iteration (unique receipt_id) so nothing is cached
        // across the P-256 signature; the body build is cheap relative to the
        // scalar multiply, but kept in the measured region to reflect a real
        // mint. RFC 6979 makes signing deterministic — no RNG on this path.
        b.iter(|| ReceiptSigner::sign(black_box(sample_body(0)), &key).unwrap());
    });
}

fn bench_verify(c: &mut Criterion) {
    let key = Es256SigningKey::generate();
    let jwks = Jwks::from_public_key(&key.public_key());
    let signed = ReceiptSigner::sign(sample_body(0), &key).unwrap();
    let compact = signed.jws_compact().to_string();
    c.bench_function("receipt/verify", |b| {
        // `verify_compact` is the canonical entry point for receipts loaded from
        // disk: base64url-decode, header check, key lookup, low-S check, ECDSA
        // verify, payload decode.
        b.iter(|| ReceiptVerifier::verify_compact(black_box(&compact), &jwks).unwrap());
    });
}

/// Build a valid, correctly-linked chain of `n` signed receipts.
fn build_chain(key: &Es256SigningKey, n: usize) -> Vec<SignedReceipt> {
    let mut chain: Vec<SignedReceipt> = Vec::with_capacity(n);
    let mut prev: Option<&SignedReceipt> = None;
    for i in 0..n {
        let body = ReceiptChain::append(prev, sample_body(i as u64));
        let signed = ReceiptSigner::sign(body, key).unwrap();
        chain.push(signed);
        prev = chain.last();
    }
    chain
}

fn bench_verify_chain(c: &mut Criterion) {
    let key = Es256SigningKey::generate();
    let jwks = Jwks::from_public_key(&key.public_key());
    let mut group = c.benchmark_group("receipt/verify_chain");
    for &n in &[1usize, 10, 100] {
        let chain = build_chain(&key, n);
        // Verification is linear in the chain length (one ECDSA verify + one
        // SHA-256 link check per receipt); the per-element throughput should be
        // roughly flat across n.
        group.throughput(criterion::Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &chain, |b, chain| {
            b.iter(|| verify_chain(black_box(chain), &jwks).unwrap());
        });
    }
    group.finish();
}

criterion_group!(benches, bench_sign, bench_verify, bench_verify_chain);
criterion_main!(benches);
