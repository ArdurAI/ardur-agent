# ardur-embeddings

Local text embeddings — the **dense** half of hybrid retrieval. Wraps
[`fastembed`](https://crates.io/crates/fastembed) (ONNX Runtime) so embeddings are
computed on-device: no per-embed network call, no API key, just a one-time model
download cached on disk.

## Quickstart

```rust
use ardur_embeddings::{Embedder, FastEmbedEmbedder, ModelChoice};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
// Default model: BGE-small-en-v1.5 (384-dim). Override with EMBED_MODEL.
let embedder = FastEmbedEmbedder::new(ModelChoice::BgeSmallEnV15)?;

let vecs = embedder
    .embed(vec!["the quick brown fox".into(), "a lazy dog".into()])
    .await?;

assert_eq!(vecs.len(), 2);
assert_eq!(vecs[0].len(), embedder.dimension()); // 384
# Ok(())
# }
```

## Models

`ModelChoice::from_env()` reads the `EMBED_MODEL` environment variable
(case-insensitive); unset or unrecognized falls back to the default.

| `EMBED_MODEL`      | `ModelChoice`     | Dim |
| ------------------ | ----------------- | --- |
| `bge-small-en-v1.5`| `BgeSmallEnV15` *(default)* | 384 |
| `gte-base-en-v1.5` | `GteBaseEnV15`    | 768 |
| `all-minilm-l6-v2` | `AllMiniLmL6V2`   | 384 |

## Testing without a model

`MockEmbedder` implements the same `Embedder` trait deterministically and with no
dependencies — same text always maps to the same L2-normalized vector. Use it to
exercise retrieval/fusion logic in unit tests without downloading a real model.

```rust
use ardur_embeddings::{Embedder, MockEmbedder};
# async fn run() {
let e = MockEmbedder::new(384);
let v = e.embed(vec!["hello".into()]).await.unwrap();
# }
```

The live test that downloads BGE-small is gated behind `EMBEDDINGS_LIVE_TEST=1`:

```sh
EMBEDDINGS_LIVE_TEST=1 cargo test -p ardur-embeddings
```
