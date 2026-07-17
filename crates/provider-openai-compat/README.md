# ardur-provider-openai-compat (§12.5)

A [`Provider`](../provider-runtime) backend for services that expose an
**OpenAI-compatible** `POST /chat/completions` endpoint.

## Why

OpenAI-compatible endpoints are the common denominator across OpenAI proper,
hosted gateways, and self-hosted servers such as vLLM. This crate lets Ardur use
those endpoints without adding one provider crate per vendor. The requested
model string passes through unchanged in `CompletionRequest::model`.

## Usage

```rust
use std::sync::Arc;
use ardur_provider_openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use ardur_provider_runtime::{ModelId, Provider, ProviderRegistry};

// From OPENAI_COMPAT_API_KEY, or OPENAI_API_KEY as a fallback:
let provider = OpenAiCompatProvider::from_env(ModelId::new("gpt-4o-mini"))?;

// …or from an explicit config (overridable base URL):
let provider = OpenAiCompatProvider::new(
    OpenAiCompatConfig::new("sk-or-...")
        .base_url("https://api.openai.com/v1"),
    ModelId::new("gpt-4o-mini"),
);

// It plugs into the generic registry like any other provider:
let mut registry = ProviderRegistry::new();
registry.register(Arc::new(provider)); // registered under id "openai-compat"
```

The model on each `CompletionRequest` selects which upstream model the endpoint
routes to; the `model_id` passed at construction is only the runtime's default.

## Supported models

The model string is **opaque** and passes through unchanged; the configured
endpoint validates it against its own catalog. Representative examples:

| Slug | Notes |
|---|---|
| `gpt-4o-mini` | OpenAI proper |
| `llama-3.1-8b-instruct` | Example self-hosted vLLM-style catalog |
| `meta-llama/llama-3.1-8b-instruct` | Example hosted gateway catalog |

## Auth and base URL

Every request sends `Authorization: Bearer <api-key>`.

Environment config:

| Variable | Purpose |
|---|---|
| `OPENAI_COMPAT_API_KEY` | Preferred API key for compatible endpoints |
| `OPENAI_API_KEY` | Fallback API key for OpenAI proper |
| `OPENAI_COMPAT_BASE_URL` | Optional base URL, default `https://api.openai.com/v1` |
| `OPENAI_COMPAT_TIMEOUT_SECS` | Optional positive whole-second timeout |

`OPENAI_COMPAT_BASE_URL` must use HTTPS unless it targets loopback HTTP
(`localhost`, `127.0.0.1`, or `::1`) for local development.

## Cost reporting

Token counts from `usage: { prompt_tokens, completion_tokens, total_tokens }`
populate the billed `CostTuple`. If a gateway emits the non-standard
`usage.cost` dollar field, it is mapped to whole US cents (`round(cost * 100)`).
When `cost` is absent the call is billed `0` cents; this generic adapter does
not reconstruct vendor-specific pricing from a rate card.

## Error mapping

| HTTP | `ProviderError` |
|---|---|
| 401 / 403 | `Unauthorized` |
| 429 | `RateLimited { retry_after_ms }` (from `Retry-After`) |
| 400 | `InvalidRequest(message)` |
| 404 | `ModelNotAvailable(model)` |
| other | `Upstream("HTTP <code>: <message> (code: …)")` |

The common `{ "error": { "message", "code" } }` body is parsed and its message
and code are surfaced in the mapped error.

## Not in Phase 1

- **Inbound OpenAI-compatible server** — `ardur serve --openai-compatible`
  remains a separate §12.5 surface.
- **Custom-provider plugin lifecycle** — install/update/remove and SecretRef
  credential custody remain separate §8.4 integration work.
- **Vendor pricing catalog** — cost is trustworthy only when the endpoint
  reports it or a future provider-specific rate table is added.
