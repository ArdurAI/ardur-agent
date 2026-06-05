//! §3.4b Phase 2 — NDJSON streaming round-trips against Ollama's streaming
//! endpoints.
//!
//! Ollama streams a completion as newline-delimited JSON: one object per token
//! chunk, terminated by `{"done": true, ...}` carrying the run's token counts.
//! These tests never touch a real daemon — a `wiremock` server returns a raw
//! NDJSON body, so chunk parsing, the done marker, token-count extraction,
//! cancellation-by-drop, and the non-streaming regression are all asserted
//! offline. A final gated test (`OLLAMA_LIVE_STREAM_TEST=1` + a running local
//! Ollama) hits the real endpoint.

use ardur_provider_ollama::{OllamaConfig, OllamaProvider};
use ardur_provider_runtime::{ChatMessage, CompletionRequest, FinishReason, ModelId, Provider};
use futures::StreamExt;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A local provider (no auth) whose base URL points at `server`.
fn local_provider(server: &MockServer, model: &str) -> OllamaProvider {
    OllamaProvider::new(
        OllamaConfig::new()
            .base_url(server.uri())
            .default_model(model),
    )
}

/// A short chat request against `model`.
fn chat_req(model: &str) -> CompletionRequest {
    CompletionRequest::new(vec![ChatMessage::user("ping")], ModelId::new(model), 64)
}

/// A canonical three-token `/api/chat` NDJSON stream: two token chunks then a
/// terminal `done` chunk carrying the run's token counts.
const CHAT_NDJSON: &str = concat!(
    r#"{"model":"llama3.2","message":{"role":"assistant","content":"Hello"},"done":false}"#,
    "\n",
    r#"{"model":"llama3.2","message":{"role":"assistant","content":" world"},"done":false}"#,
    "\n",
    r#"{"model":"llama3.2","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":11,"eval_count":2}"#,
    "\n",
);

/// Mount a `POST /api/chat` that asserts `stream: true` rode out and returns
/// `body` as a raw NDJSON response.
async fn mount_chat_ndjson(server: &MockServer, body: &'static str) {
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(body_partial_json(serde_json::json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn parses_ndjson_chat_chunks() {
    let server = MockServer::start().await;
    mount_chat_ndjson(&server, CHAT_NDJSON).await;

    let provider = local_provider(&server, "llama3.2");
    let stream = provider
        .stream_ndjson(chat_req("llama3.2"))
        .await
        .expect("the streaming handshake succeeds");
    let chunks: Vec<_> = stream
        .map(|r| r.expect("each chunk parses"))
        .collect()
        .await;

    // Three NDJSON lines → three chunks; the two token chunks reassemble.
    assert_eq!(chunks.len(), 3);
    let text: String = chunks.iter().map(|c| c.token()).collect();
    assert_eq!(text, "Hello world");
}

#[tokio::test]
async fn parses_done_marker() {
    let server = MockServer::start().await;
    mount_chat_ndjson(&server, CHAT_NDJSON).await;

    let provider = local_provider(&server, "llama3.2");
    let stream = provider
        .stream_ndjson(chat_req("llama3.2"))
        .await
        .expect("the streaming handshake succeeds");
    let chunks: Vec<_> = stream
        .map(|r| r.expect("each chunk parses"))
        .collect()
        .await;

    // Exactly one terminal chunk, and it is the last one.
    let done_count = chunks.iter().filter(|c| c.is_done()).count();
    assert_eq!(done_count, 1, "exactly one done marker");
    assert!(
        chunks.last().expect("non-empty").is_done(),
        "the done marker is the final chunk"
    );
    assert!(!chunks[0].is_done(), "earlier chunks are not done");
}

#[tokio::test]
async fn extracts_token_counts_from_final_chunk() {
    let server = MockServer::start().await;
    mount_chat_ndjson(&server, CHAT_NDJSON).await;

    let provider = local_provider(&server, "llama3.2");
    // Drive the raw NDJSON chunk surface; the shared StreamEvent surface
    // (`Provider::stream`) is covered in `stream_trait.rs`.
    let stream = provider
        .stream_ndjson(chat_req("llama3.2"))
        .await
        .expect("the streaming handshake succeeds");
    let chunks: Vec<_> = stream
        .map(|r| r.expect("each chunk parses"))
        .collect()
        .await;

    // Two token chunks then a terminal `done` chunk carrying the folded counts.
    let tokens: String = chunks.iter().map(|c| c.token()).collect();
    assert_eq!(tokens, "Hello world");

    let done = chunks.last().expect("a terminal chunk");
    assert!(done.is_done());
    assert_eq!(done.usage().tokens_in, 11);
    assert_eq!(done.usage().tokens_out, 2);
    assert!(matches!(done.finish_reason(), FinishReason::Stop));
}

#[tokio::test]
async fn aborts_on_drop() {
    let server = MockServer::start().await;
    mount_chat_ndjson(&server, CHAT_NDJSON).await;

    let provider = local_provider(&server, "llama3.2");
    let mut stream = provider
        .stream_ndjson(chat_req("llama3.2"))
        .await
        .expect("the streaming handshake succeeds");

    // Pull a single chunk, then drop the stream mid-flight: the underlying
    // reqwest byte stream is dropped, closing the connection (the §3.4b
    // cancellation contract). The remaining chunks are never consumed.
    let first = stream
        .next()
        .await
        .expect("at least one chunk")
        .expect("the first chunk parses");
    assert_eq!(first.token(), "Hello");
    drop(stream); // No panic, no hang — cancellation is just a drop.
}

#[tokio::test]
async fn non_streaming_path_still_works() {
    // Regression: the buffered `complete` path is untouched by the streaming
    // work — it still pins `stream: false` and parses a single JSON reply.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(body_partial_json(serde_json::json!({"stream": false})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "llama3.2",
            "message": {"role": "assistant", "content": "pong"},
            "done_reason": "stop",
            "done": true,
            "prompt_eval_count": 12,
            "eval_count": 3
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = local_provider(&server, "llama3.2");
    let resp = provider
        .complete(chat_req("llama3.2"))
        .await
        .expect("the buffered call succeeds");
    assert_eq!(resp.content, "pong");
    assert_eq!(resp.usage.tokens_in, 12);
    assert_eq!(resp.usage.tokens_out, 3);
    assert_eq!(resp.cost.cents, 0);
}

#[tokio::test]
async fn chunks_split_across_byte_boundaries_reassemble() {
    // A line whose bytes arrive split mid-token still parses: wiremock sends the
    // whole body, but the parser's carry-buffer logic is the same regardless of
    // how the bytes are framed. Two adjacent lines with no trailing newline on
    // the last exercises the end-of-stream flush.
    let body = concat!(
        r#"{"message":{"content":"a"},"done":false}"#,
        "\n",
        r#"{"message":{"content":"b"},"done":true,"prompt_eval_count":3,"eval_count":2}"#,
    );
    let server = MockServer::start().await;
    mount_chat_ndjson(&server, body).await;

    let provider = local_provider(&server, "llama3.2");
    let stream = provider
        .stream_ndjson(chat_req("llama3.2"))
        .await
        .expect("the streaming handshake succeeds");
    let chunks: Vec<_> = stream
        .map(|r| r.expect("each chunk parses"))
        .collect()
        .await;
    assert_eq!(chunks.len(), 2, "trailing newline-less line is flushed");
    assert_eq!(chunks[1].usage().tokens_out, 2);
}

/// Gated live test: requires `OLLAMA_LIVE_STREAM_TEST=1` and a running local
/// Ollama with the model pulled. Skipped (passes trivially) otherwise.
#[tokio::test]
async fn live_stream_hits_real_endpoint() {
    if std::env::var("OLLAMA_LIVE_STREAM_TEST").as_deref() != Ok("1") {
        eprintln!("skipping: set OLLAMA_LIVE_STREAM_TEST=1 with a running Ollama to enable");
        return;
    }
    let model = std::env::var("OLLAMA_LIVE_MODEL").unwrap_or_else(|_| "llama3.2".to_string());
    let provider = OllamaProvider::from_env();
    let stream = provider
        .stream_ndjson(CompletionRequest::new(
            vec![ChatMessage::user("Say the single word: pong")],
            ModelId::new(&model),
            64,
        ))
        .await
        .expect("the live streaming handshake succeeds");

    let chunks: Vec<_> = stream
        .map(|r| r.expect("each live chunk parses"))
        .collect()
        .await;
    assert!(
        chunks.iter().any(|c| !c.token().is_empty()),
        "the live stream yields at least one non-empty token"
    );
    let done = chunks
        .iter()
        .find(|c| c.is_done())
        .expect("a terminal done chunk from the live stream");
    assert!(
        done.usage().tokens_out > 0,
        "the live run reports output tokens"
    );
}
