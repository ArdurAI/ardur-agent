# Benchmarks — hot-path baselines

Ardur's performance-sensitive inner loops are measured by a [criterion][criterion]
harness in `crates/benches` (a bench-only workspace member; `publish = false`).
Run them with:

```bash
# all four targets
cargo bench -p ardur-benches --benches

# one target
cargo bench -p ardur-benches --bench fusion

# quick pass (shorter warm-up / measurement, noisier)
cargo bench -p ardur-benches --benches -- --warm-up-time 1 --measurement-time 3
```

## What is measured (and what is not)

The harness depends only on the **public API** of the crates it measures, so it
never couples to their internals or forces an in-flight crate to rebuild. Four
targets map to the hot paths:

| Target             | Crate(s)                          | Covers |
|--------------------|-----------------------------------|--------|
| `fusion`           | `ardur-fusion`                    | RRF / relative-score / distance-score fusion |
| `memory_retrieval` | `ardur-bm25-index`, `ardur-memory`, `ardur-embeddings`, `ardur-fusion` | the **hermetic** half of hybrid recall |
| `receipt`          | `ardur-receipt`                   | JWS-ES256 sign / verify / chain verify |
| `cost_gate`        | `ardur-cost-gate`                 | admit → finalize → commit, and the deny fast path |

**Deliberately excluded — not hermetic.** Production hybrid recall
(`HybridMemoryRetriever`) also runs a *dense* half: the query is embedded by a
downloaded ONNX model (`fastembed`) and ANN-searched in a running Qdrant service.
Neither dependency is available in CI, and both would make baselines
irreproducible, so they are not benchmarked here. The `MockEmbedder` stands in
for the *shape* of the embed step, not the real model's cost; the dense vector
search has no hermetic stand-in and is omitted. A full end-to-end
`HybridMemoryRetriever` bench belongs in an integration harness with a
service-backed fixture, not this unit-level suite.

Likewise, a full `fused-runtime` turn is dominated by the provider network call
and is not hermetic; the parts of a turn that *are* CPU-bound and hermetic — the
cost-gate admission cycle and the receipt mint/verify — are covered by the
`cost_gate` and `receipt` targets.

## Baselines

Captured 2026-07-14 on an Apple-silicon macOS dev host (`bench` profile,
`--warm-up-time 1 --measurement-time 3`). **These are order-of-magnitude
baselines, not a spec.** The host is a shared laptop and the run-to-run variance
is high (±15–20% on unchanged code — see the caveat below); treat the numbers as
"what dominates, and roughly how it scales", and rely on criterion's own
`--baseline` A/B (same host, back-to-back) for any regression gate.

### `fusion` (per hybrid recall)

`n` is the per-retriever candidate-list length. The realistic hybrid pool is
`candidate_pool(top_k) = max(top_k * 4, 32)`, so `n = 32` (small `top_k`),
`n = 40` (`top_k = 10`), and `n = 400` (`top_k = 100`) are the live sizes; 1024
shows scaling.

| Benchmark                     | median  |
|-------------------------------|---------|
| `reciprocal_rank/32`          | ~4.1 µs |
| `reciprocal_rank/40`          | ~5.7 µs |
| `reciprocal_rank/400`         | ~63 µs  |
| `reciprocal_rank/1024`        | ~169 µs |
| `relative_score/32`           | ~4.1 µs |
| `relative_score/400`          | ~60 µs  |
| `distance_score/32`           | ~4.0 µs |
| `distance_score/400`          | ~62 µs  |

(Medians after the RRF allocation fix below; fusion is `O(n log n)` per list,
dominated by the two rank sorts plus the final fused-map sort.)

### `memory_retrieval`

| Benchmark                      | median   | notes |
|--------------------------------|----------|-------|
| `bm25_query/100`               | ~18 µs   | Tantivy BM25 search, 100-doc corpus |
| `bm25_query/1000`              | ~32 µs   | 1000-doc corpus |
| `in_process_search/100`        | ~49 µs   | offline lexical fallback, full scan |
| `in_process_search/1000`       | ~455 µs  | **`O(n)` in corpus size** |
| `in_process_search/10000`      | ~5.6 ms  | clones every matching record before top-k |
| `mock_embed`                   | ~5 µs    | deterministic stand-in, **not** the real model |
| `fuse_hybrid`                  | ~9 µs    | RRF over real BM25 + synthetic dense, 40 each |

### `receipt` (ECDSA P-256, per receipt)

| Benchmark              | median   | notes |
|------------------------|----------|-------|
| `sign`                 | ~253 µs  | one P-256 signature (RFC 6979, no RNG) + JSON/base64 framing |
| `verify`               | ~358 µs  | one P-256 verify + low-S check + framing |
| `verify_chain/1`       | ~293 µs  | one verify + one link check |
| `verify_chain/10`      | ~3.4 ms  | linear: ~340 µs/receipt |
| `verify_chain/100`     | ~44 ms   | linear: ~440 µs/receipt |

These are the intrinsic cost of the P-256 primitive; they are a **regression
guard**, not an optimization target. Any speedup here must not weaken the low-S
malleability rejection or the verify-before-trust ordering in `verify_chain`.

### `cost_gate`

| Benchmark                    | median   | notes |
|------------------------------|----------|-------|
| `admit_finalize_commit`      | ~2.5 µs  | full admission cycle: 3 lock regions, 2 map ops, 1 UUIDv4 |
| `admit_denied_ceiling`       | ~0.36 µs | stage-2 rejection, no budget touched |

The gate is already in the low-microsecond range and is not a hotspot; the
baseline exists to catch a future regression (e.g. a lock-contention or
allocation change).

## Findings

### Applied — RRF: clone `doc_id` only on first insertion

`reciprocal_rank_fusion` accumulated fused scores with
`*fused.entry(doc_id.clone()).or_insert(0.0) += contribution`, which **clones the
`doc_id` `String` on every insertion — including when the key already exists**.
The overlap between the dense and sparse lists is exactly the case RRF exists to
reward, so those repeat hits are common. Replacing the `entry(clone)` with a
`get_mut`-or-`insert` reduces clones from `O(total entries)` to `O(distinct
entries)` — behaviour is identical (`entry().or_insert(0.0) += c` ≡ "add if
present, else insert `c`"), and all `ardur-fusion` unit tests still pass.

A/B on the same host (criterion `--save-baseline` / `--baseline`, back-to-back):
the untouched `relative_score` / `distance_score` benches act as a control group
and swing ±15–20% (the host's noise floor), while `reciprocal_rank/400` and
`/1024` show a ~25–30% point-estimate improvement (`p < 0.05`), and the small-`n`
cases show no change (fixed sort overhead dominates there). The win is not
cleanly separable from host noise at a precise percentage, but the direction is
consistent and the change is strictly less work per hit — so it is applied.

### Noted, not changed — `InMemoryMemoryRuntime::search` is `O(n)` and clone-heavy

The in-process lexical search scans every record and `clone`s each match into a
scored tuple *before* truncating to `top_k` (`~5.6 ms` at 10k records). Deferring
the clone until after top-k selection would cut it materially. It is **left
untouched** here because (a) it is the explicitly Phase-1 offline fallback (the
production path is `HybridMemoryRetriever`, gated on Qdrant), and (b) the
`ardur-memory` crate is under active development in another lane — a surgical
bench PR should not reach into it. Flagged for that lane.

## Methodology notes

- Functions that consume their input (the fusion routines) are benched with
  `iter_batched`, so the per-iteration `clone` of the input is untimed setup and
  the measured region is the routine alone.
- Async surfaces (BM25, the cost-gate, the embedder) are driven on a
  current-thread tokio runtime inside the measured closure.
- Corpora are deterministic synthetic text (a fixed LCG over a small vocabulary)
  so runs are comparable across machines and dates.
- For a trustworthy regression check, run `--save-baseline` before a change and
  `--baseline` after, **on the same host back-to-back** — cross-run/cross-host
  comparisons are dominated by environment noise on a shared laptop.

[criterion]: https://github.com/bheisler/criterion.rs
