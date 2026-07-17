# ardur-memory-qdrant

A durable, **Qdrant-backed** implementation of the §7.0 `MemoryRuntime` trait —
plus a **hybrid** (dense + sparse) retriever over the same store.

The §7.0 Phase 1 store (`ardur-memory::InMemoryMemoryRuntime`) lives only in
process and is lost on restart. This crate upserts every bi-temporal record as a
Qdrant point, so the memory substrate survives a process — or pod — restart. It
implements the *same* `MemoryRuntime` trait, so the fused runtime and the server
select it behind the existing `Arc<dyn MemoryRuntime + Send + Sync>` seam with no
call-site changes (`ARDUR_MEMORY=qdrant`).

## What's in the box

| Type | Role |
| ---- | ---- |
| `QdrantMemoryConfig` | connection + collection settings (`from_env` + builder) |
| `QdrantPayload` | the bi-temporal payload schema stored on each point |
| `QdrantMemoryRuntime` | the durable `MemoryRuntime`; embeds each record, vector-searches, snapshots |
| `HybridMemoryRetriever` | dense (vector) + sparse (BM25) recall, fused with reciprocal-rank fusion |
| `searchable_text` | the one string a record is embedded + lexically-indexed on |

## Real embeddings

`QdrantMemoryRuntime` embeds each record's `searchable_text` (the fact's
`predicate object`, falling back to the payload text) through an attached
[`Embedder`](../embeddings). Attach one with the builder:

```rust
use std::sync::Arc;
use ardur_memory_qdrant::{FastEmbedEmbedder, QdrantMemoryConfig, QdrantMemoryRuntime};

# fn run() -> Result<(), Box<dyn std::error::Error>> {
let embedder = Arc::new(FastEmbedEmbedder::from_env()?); // EMBED_MODEL, default BGE-small (384-d)
let runtime = QdrantMemoryRuntime::connect(QdrantMemoryConfig::from_env())?
    .with_embedder(embedder); // realigns the collection dim to the model's dim
runtime.init()?;             // create the collection *after* attaching the embedder
# Ok(()) }
```

Without an embedder the runtime keeps the legacy placeholder vector — the
bi-temporal `at_time` / `history_of` reads still work (they scroll by payload
filter), only vector *search* is meaningless. `with_embedder` realigns the
collection's vector dimension to the model's output dim, so call it **before**
`init`.

`QdrantMemoryConfig::default_embed_model` (env `EMBED_MODEL`) records which model
the store should use; resolve it to a concrete `Embedder` via
`ardur_embeddings::ModelChoice` and attach it as above.

## Hybrid retrieval

`HybridMemoryRetriever` runs a query through two complementary retrievers and
fuses them:

- **dense** — the query is embedded and matched against each record's stored
  vector via Qdrant ANN search. Matches on *meaning*.
- **sparse** — a BM25 lexical index ([`ardur-bm25-index`](../bm25-index)) matches
  on *terms*.

Their scores are incomparable (cosine in `[-1, 1]`, BM25 in `[0, ∞)`), so the
ranked lists are combined with rank-based reciprocal-rank fusion
([`ardur-fusion`](../fusion)). `record` writes to **both** backends; `search`
fuses and hydrates the top-`k` to full `MemoryRecord`s (invalidation tombstones
are never returned).

```rust
use std::sync::Arc;
use ardur_memory_qdrant::{
    Bm25Index, FastEmbedEmbedder, HybridMemoryRetriever, QdrantMemoryConfig, QdrantMemoryRuntime,
};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let embedder = Arc::new(FastEmbedEmbedder::from_env()?);
let qdrant = QdrantMemoryRuntime::connect(QdrantMemoryConfig::from_env())?;
let bm25 = Bm25Index::new(None)?; // in-memory; pass Some(dir) to persist

let hybrid = HybridMemoryRetriever::new(qdrant, bm25, embedder);
hybrid.qdrant().init()?; // init after the embedder is attached

// hybrid.record(rec).await?;
let hits = hybrid.search("favorite hot beverage", 5).await?;
# Ok(()) }
```

The retriever shares one embedder with the underlying runtime, so a record and a
query are always embedded by the same model.

## Tests

```sh
# Pure unit tests (fusion wiring, searchable_text, config) — no services:
cargo test -p ardur-memory-qdrant

# Live integration (needs a Qdrant; uses the deterministic MockEmbedder):
docker run -p 6334:6334 qdrant/qdrant
QDRANT_INTEGRATION_TEST=1 cargo test -p ardur-memory-qdrant

# Semantic tests additionally download + run the real BGE-small model:
QDRANT_INTEGRATION_TEST=1 EMBEDDINGS_LIVE_TEST=1 cargo test -p ardur-memory-qdrant
```
