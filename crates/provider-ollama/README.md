# ardur-provider-ollama (§3.3)

A [`Provider`](../provider-runtime) backend for [Ollama](https://ollama.com) —
open models served either **locally** (a daemon on `http://localhost:11434`,
no auth) or from Ollama's **hosted cloud** (`https://ollama.com`, Bearer auth).
Both speak the same native `POST /api/chat` endpoint, so one crate covers both.

## Why

Ollama unblocks dogfooding ardur-agent against open models with zero spend: pull
a model once (`ollama pull llama3.2`) and the local daemon serves it for free,
or point the same backend at the cloud with an API key for models too big to run
on a laptop. One endpoint, local or cloud — selected by the base URL.

## Usage

```rust
use std::sync::Arc;
use ardur_provider_ollama::{OllamaConfig, OllamaProvider};
use ardur_provider_runtime::{Provider, ProviderRegistry};

// A local daemon (no auth), defaulting to the `llama3.2` model:
let provider = OllamaProvider::new(OllamaConfig::new());

// …the hosted cloud (Bearer auth), with an explicit default model:
let provider = OllamaProvider::new(
    OllamaConfig::new()
        .base_url(ardur_provider_ollama::CLOUD_BASE_URL)
        .api_key("sk-…")
        .default_model("gpt-oss:120b"),
);

// …or read the connection from the environment (see below):
let provider = OllamaProvider::from_env();

// It plugs into the generic registry like any other provider:
let mut registry = ProviderRegistry::new();
registry.register(Arc::new(provider)); // registered under id "ollama"
```

The model on each `CompletionRequest` selects which Ollama model runs the turn —
the default model passed at construction is only the runtime's fallback.

## Configuration from the environment

`OllamaConfig::from_env()` / `OllamaProvider::from_env()` read:

- `OLLAMA_API_KEY` — the cloud Bearer key. Unset/empty ⇒ no auth (local).
- `OLLAMA_BASE_URL` — the base URL. When unset, it defaults to
  `https://ollama.com` if a key was found (the cloud) and
  `http://localhost:11434` otherwise (a local daemon).

`from_env()` never fails: a local Ollama needs no credentials, so there is no
"missing key" error.

## Supported models

The model string is **opaque** and passes through unchanged; Ollama validates it
against what is installed locally (`ollama list`) or hosted in the cloud. Any
pulled model name works. Representative examples:

| Name | Notes |
|---|---|
| `llama3.2` | Meta Llama 3.2 (the crate default) |
| `qwen2.5` | Alibaba Qwen 2.5 |
| `mistral` | Mistral 7B |
| `gemma2` | Google Gemma 2 |
| `deepseek-r1` | DeepSeek R1 |
| `gpt-oss:20b` | tag-qualified size variant |

## Auth

A request sends `Authorization: Bearer <api-key>` **only when an API key is
set** (the cloud). A local daemon takes no credentials, so the header is omitted.

## Cost reporting

Ollama reports **no dollar cost** — a local daemon is free and the cloud bills
out-of-band. Token counts come from the response's `prompt_eval_count` (input)
and `eval_count` (output) and populate the billed `CostTuple`, but every call is
priced at `0` cents.

## Error mapping

| Condition | `ProviderError` |
|---|---|
| 401 / 403 | `Unauthorized` |
| 429 | `RateLimited { retry_after_ms }` (from `Retry-After`) |
| 400 | `InvalidRequest(message)` |
| 404 | `ModelNotAvailable(model)` |
| 5xx / other | `Upstream("HTTP <code>: <message>")` |
| connection refused | `Upstream(…)` with a hint to start `ollama serve` |
| other transport error | `NetworkFailure(message)` |

Ollama's `{ "error": "<message>" }` body is parsed and its message surfaced in
the mapped error.

## Not in Phase 1

- **Streaming** — every request sends `stream: false`;
  `supports_streaming()` is `false`. (Phase 2.)
- **Tool-call parsing** — the message's `tool_calls` field is not decoded yet.
  (Phase 2.)
