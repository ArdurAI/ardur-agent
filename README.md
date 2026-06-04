# Ardur

**The open-source agent substrate** — a Rust runtime for building trustable, observable, and composable AI agent products.

Ardur gives agents three primitives that make their behavior auditable end to end: **cap-tokens** scope every action to an explicit, revocable capability so an agent can only do what it was granted; **receipt-chains** record each action as a signed, tamper-evident link so the full history of what an agent did is reconstructable and verifiable; and **cost-tuples** attach a structured cost (tokens, money, time, risk) to every operation so spend and budget are first-class, not an afterthought. Together they turn an agent from an opaque process into a substrate you can reason about, govern, and compose.

## Status

**Early — under active design and development. APIs are unstable and will change without notice.** Do not depend on this for production work yet. Follow the design corpus (below) to track where it's going.

## Key concepts

- **Cap-token** — a scoped, revocable capability that authorizes a specific action. No ambient authority; an agent holds exactly the caps it was granted.
- **Receipt-chain** — an append-only, signed chain of receipts. Every action emits a receipt linked to its predecessor, so the audit trail is tamper-evident and replayable.
- **Cost-tuple** — a structured cost attached to every operation, making token/money/time/risk budgets observable and enforceable at the substrate level.

## Hybrid retrieval

Foundation crates for retrieving an agent's memories by both meaning and exact
terms, then fusing the two rankings:

- [`crates/embeddings`](crates/embeddings) — local text embeddings via fastembed
  (ONNX), the dense half. Default model BGE-small-en-v1.5 (384-dim).
- [`crates/bm25-index`](crates/bm25-index) — BM25 lexical search via Tantivy, the
  sparse half. In-memory or file-backed.
- [`crates/fusion`](crates/fusion) — reciprocal-rank, relative-score, and
  distance-score fusion (ported from LlamaIndex) to combine the two result lists.

## Quick links

- Documentation — _coming soon_ (`docs/` in-repo)
- Plan corpus / design blueprints — _not yet published; tracked privately during early design_
- Design notes — _coming soon_

## Build & test

> The Rust workspace is not yet scaffolded (pending the §0.0 Phase-0 scaffold). These commands are placeholders and will be filled in once the workspace and CI land.

```sh
cargo build      # placeholder
cargo test       # placeholder
cargo clippy     # placeholder
```

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) — note that **DCO sign-off** and **SSH-signed commits** are required. Please also read the [Code of Conduct](CODE_OF_CONDUCT.md) and the security policy in [SECURITY.md](SECURITY.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE) — chosen for its explicit patent grant and enterprise clarity.
