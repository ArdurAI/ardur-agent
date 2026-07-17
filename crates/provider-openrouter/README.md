# ardur-provider-openrouter (§3.2)

A [`Provider`](../provider-runtime) backend for [OpenRouter](https://openrouter.ai) —
a multi-model gateway that exposes a single **OpenAI-compatible**
`POST /chat/completions` endpoint routing to many upstream providers behind one
API key.

## Why

OpenRouter unblocks dogfooding ardur-agent against cheap or free models without
wiring a separate backend per vendor. One API key, one endpoint, dozens of
models — selected per request by passing the model slug through the request's
`ModelId`.

## Usage

```rust
use std::sync::Arc;
use ardur_provider_openrouter::{OpenRouterConfig, OpenRouterProvider};
use ardur_provider_runtime::{ModelId, Provider, ProviderRegistry};

// From the OPENROUTER_API_KEY environment variable:
let provider = OpenRouterProvider::from_env(ModelId::new("anthropic/claude-3.5-sonnet"))?;

// …or from an explicit config (overridable base URL + attribution headers):
let provider = OpenRouterProvider::new(
    OpenRouterConfig::new("sk-or-...")
        .referer("https://github.com/ArdurAI/ardur-agent")
        .title("Ardur Agent"),
    ModelId::new("openai/gpt-4o"),
);

// It plugs into the generic registry like any other provider:
let mut registry = ProviderRegistry::new();
registry.register(Arc::new(provider)); // registered under id "openrouter"
```

The model on each `CompletionRequest` selects which upstream model OpenRouter
routes to — the `model_id` passed at construction is only the runtime's default.

## Supported models

The model string is **opaque** and passes through unchanged; OpenRouter
validates it against its catalog. Any slug from
<https://openrouter.ai/models> works. Representative examples:

| Slug | Notes |
|---|---|
| `anthropic/claude-3.5-sonnet` | Anthropic via the gateway |
| `openai/gpt-4o` | OpenAI |
| `google/gemini-flash-1.5` | Google |
| `meta-llama/llama-3.1-8b-instruct:free` | free tier (`:free` suffix) |
| `mistralai/mistral-7b-instruct` | Mistral |
| `deepseek/deepseek-chat` | DeepSeek |

## Auth & headers

Every request sends:

- `Authorization: Bearer <api-key>`
- `HTTP-Referer: https://github.com/ArdurAI/ardur-agent` (default; OpenRouter's
  recommended attribution header, overridable)
- `X-Title: Ardur Agent` (default, overridable)

## Cost reporting

OpenRouter returns `usage: { prompt_tokens, completion_tokens, total_tokens,
cost }`. Token counts populate the billed `CostTuple`, and the dollar `cost` is
mapped to whole US cents (`round(cost × 100)`). When `cost` is absent the call
is billed `0` cents — Phase 1 does not reconstruct cost from a rate card.

## Error mapping

| HTTP | `ProviderError` |
|---|---|
| 401 / 403 | `Unauthorized` |
| 429 | `RateLimited { retry_after_ms }` (from `Retry-After`) |
| 400 | `InvalidRequest(message)` |
| 404 | `ModelNotAvailable(model)` |
| other | `Upstream("HTTP <code>: <message> (code: …)")` |

OpenRouter's `{ "error": { "message", "code" } }` body is parsed and its message
+ code surfaced in the mapped error.

## Not in Phase 1

- **Streaming** — every request sends `stream: false`;
  `supports_streaming()` is `false`. (Phase 2.)
- **Tool-call parsing** — a `tool_calls` finish reason surfaces as
  `FinishReason::ToolUse(vec![])`; the blocks are not decoded yet. (Phase 2.)
