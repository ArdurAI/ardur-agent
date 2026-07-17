# ardur-bm25-index

BM25 lexical search — the **sparse** half of hybrid retrieval. Wraps
[`tantivy`](https://crates.io/crates/tantivy) with a fixed two-field schema and a
small async surface. Where a dense embedding search matches on *meaning*, BM25
matches on *terms*: it rewards documents containing the query's exact words.

## Quickstart

```rust
use ardur_bm25_index::Bm25Index;

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
// None = in-memory; Some(dir) = file-backed (persists across restarts).
let mut index = Bm25Index::new(None)?;

index.add("doc-1".into(), "the quick brown fox".into()).await?;
index.add("doc-2".into(), "a lazy sleeping dog".into()).await?;

let hits = index.query("fox", 10).await?;
assert_eq!(hits[0].doc_id, "doc-1");
assert!(hits[0].score > 0.0);
# Ok(())
# }
```

## Schema

Two fields, fixed:

| Field    | Tantivy options        | Purpose                                  |
| -------- | ---------------------- | ---------------------------------------- |
| `doc_id` | `STRING \| STORED \| FAST` | stable id, stored to map a hit back to it |
| `text`   | `TEXT`                 | tokenized + inverted; the searchable body |

## Persistence

Passing `Some(dir)` builds (or reopens) a file-backed index. Documents committed
in one process are searchable after reopening the same directory:

```rust
use ardur_bm25_index::Bm25Index;
# async fn run(dir: std::path::PathBuf) -> Result<(), Box<dyn std::error::Error>> {
{
    let mut idx = Bm25Index::new(Some(dir.clone()))?;
    idx.add("persisted".into(), "durable document".into()).await?;
} // commit is on disk

let reopened = Bm25Index::new(Some(dir))?;
assert_eq!(reopened.query("durable", 10).await?[0].doc_id, "persisted");
# Ok(())
# }
```

## Notes

- Each `add` commits, so the document is immediately queryable — convenient for
  incremental use, at the cost of a commit per document. Batched writes are a
  future optimization.
- `add`/`query` are `async` to mirror the `Embedder` surface in `ardur-embeddings`
  (Tantivy itself is synchronous), so a hybrid retriever can `await` both halves
  uniformly.
- Pair the `ScoredDoc` results from both retrievers through
  [`ardur-fusion`](../fusion) to produce one fused ranking.
