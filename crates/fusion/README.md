# ardur-fusion

Rank/score **fusion** for hybrid retrieval. When you run a query through several
retrievers — a dense embedding search and a sparse BM25 search, say — you get
several ranked lists scored on incomparable scales. This crate combines them into
one ranking. Pure algorithm port, no I/O, no async.

Three strategies, ported faithfully from LlamaIndex's `QueryFusionRetriever`
(`llama_index/core/retrievers/fusion_retriever.py`):

| Function                  | How it combines                                        |
| ------------------------- | ------------------------------------------------------ |
| `reciprocal_rank_fusion`  | rank-based: doc at rank *r* adds `1/(r + k)`, summed. Ignores raw scores — the robust default. |
| `relative_score_fusion`   | min-max normalize each list to `[0,1]`, weight, sum.   |
| `distance_score_fusion`   | normalize against a `mean ± 3σ` band instead of min/max (steadier under outliers). |

## Quickstart

```rust
use ardur_fusion::{reciprocal_rank_fusion, ScoredDoc, DEFAULT_RRF_K};

let dense  = vec![ScoredDoc::new("a", 0.91), ScoredDoc::new("b", 0.55)];
let sparse = vec![ScoredDoc::new("b", 12.3), ScoredDoc::new("c", 3.1)];

// k = 60 (Cormack 2009), top_k = 10.
let fused = reciprocal_rank_fusion(vec![dense, sparse], DEFAULT_RRF_K, 10);

// "b" is ranked by both retrievers, so it floats to the top.
assert_eq!(fused[0].doc_id, "b");
```

Weighted relative-score fusion:

```rust
use ardur_fusion::{relative_score_fusion, ScoredDoc};

let l1 = vec![ScoredDoc::new("a", 10.0), ScoredDoc::new("b", 0.0)];
let l2 = vec![ScoredDoc::new("a", 4.0),  ScoredDoc::new("c", 1.0)];

// Weight the first retriever 0.7, the second 0.3.
let fused = relative_score_fusion(vec![l1, l2], Some(vec![0.7, 0.3]), 10);
assert_eq!(fused[0].doc_id, "a");
```

## Determinism

Ties on fused score break on `doc_id` ascending, so output is identical run to run
regardless of input map ordering — reproducible for tests and stable receipts.

## References

- Ported from LlamaIndex `fusion_retriever.py` (`_reciprocal_rerank_fusion`,
  `_relative_score_fusion` and its `dist_based` branch).
- Cormack, Clarke & Büttcher, *"Reciprocal Rank Fusion outperforms Condorcet and
  individual Rank Learning Methods"*, SIGIR 2009 — the source of RRF and its
  default `k = 60`.
