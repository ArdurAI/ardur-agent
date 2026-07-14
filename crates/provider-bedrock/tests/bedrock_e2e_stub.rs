//! §3.4 Phase 1 — wiremock round-trip against the Bedrock backend's public
//! surface: build a real [`BedrockProvider`], drive it through [`Provider`],
//! assert on the returned type and the SigV4-signed request shape. Never
//! touches real AWS — a `wiremock` server stands in for
//! `bedrock-runtime.{region}.amazonaws.com`, so CI stays green with no
//! credentials.

use ardur_provider_bedrock::{BedrockConfig, BedrockProvider};
use ardur_provider_runtime::{
    ChatMessage, CompletionRequest, FinishReason, ModelId, Provider, StreamEvent,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::StreamExt;
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn provider_for(server: &MockServer) -> BedrockProvider {
    BedrockProvider::new(
        BedrockConfig::new("AKIDEXAMPLE", "secret").base_url_override(server.uri()),
        ModelId::new("anthropic.claude-3-5-sonnet-20241022-v2:0"),
    )
}

#[tokio::test]
async fn complete_signs_and_round_trips_the_invoke_model_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/invoke",
        ))
        .and(header_exists("authorization"))
        .and(header_exists("x-amz-date"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content": [{"type": "text", "text": "pong"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 9, "output_tokens": 2}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let req = CompletionRequest::new(
        vec![ChatMessage::user("ping")],
        ModelId::new("anthropic.claude-3-5-sonnet-20241022-v2:0"),
        32,
    );
    let resp = provider.complete(req).await.expect("the call succeeds");

    assert_eq!(resp.content, "pong");
    assert!(matches!(resp.finish_reason, FinishReason::Stop));
    assert_eq!(resp.usage.tokens_in, 9);
    assert_eq!(resp.usage.tokens_out, 2);
}

#[tokio::test]
async fn tool_use_stop_reason_decodes_into_tool_calls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/invoke",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content": [{"type": "tool_use", "id": "call_1", "name": "echo", "input": {"msg": "hi"}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 5, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let req = CompletionRequest::new(
        vec![ChatMessage::user("call echo")],
        ModelId::new("anthropic.claude-3-5-sonnet-20241022-v2:0"),
        32,
    );
    let resp = provider.complete(req).await.expect("the call succeeds");

    match resp.finish_reason {
        FinishReason::ToolUse(calls) => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id, "call_1");
            assert_eq!(calls[0].name, "echo");
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

/// The standard CRC-32 (IEEE 802.3), duplicated from the crate's internal
/// `eventstream` module (not part of its public surface) so this black-box
/// test can construct a valid `application/vnd.amazon.eventstream` fixture
/// the same way a real Bedrock stream would be framed.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Build one well-formed AWS event-stream frame carrying `headers` and
/// `payload`.
fn build_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut header_bytes = Vec::new();
    for (name, value) in headers {
        header_bytes.push(name.len() as u8);
        header_bytes.extend_from_slice(name.as_bytes());
        header_bytes.push(7); // string type
        header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
        header_bytes.extend_from_slice(value.as_bytes());
    }
    let total_length = 12 + header_bytes.len() + payload.len() + 4;
    let mut frame = Vec::with_capacity(total_length);
    frame.extend_from_slice(&(total_length as u32).to_be_bytes());
    frame.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32(&frame[0..8]).to_be_bytes());
    frame.extend_from_slice(&header_bytes);
    frame.extend_from_slice(payload);
    let message_crc = crc32(&frame);
    frame.extend_from_slice(&message_crc.to_be_bytes());
    frame
}

/// Build a "chunk" event-stream frame wrapping `anthropic_event_json` as
/// Bedrock does: base64-encoded inside a `{"bytes": "..."}` payload.
fn chunk_frame(anthropic_event_json: &str) -> Vec<u8> {
    let b64 = BASE64.encode(anthropic_event_json);
    let payload = format!(r#"{{"bytes":"{b64}"}}"#);
    build_frame(
        &[(":message-type", "event"), (":event-type", "chunk")],
        payload.as_bytes(),
    )
}

#[tokio::test]
async fn stream_signs_decodes_and_yields_real_incremental_events() {
    let server = MockServer::start().await;
    let mut body = Vec::new();
    body.extend_from_slice(&chunk_frame(
        r#"{"type":"message_start","message":{"usage":{"input_tokens":9}}}"#,
    ));
    body.extend_from_slice(&chunk_frame(
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    ));
    body.extend_from_slice(&chunk_frame(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"pong"}}"#,
    ));
    body.extend_from_slice(&chunk_frame(r#"{"type":"content_block_stop","index":0}"#));
    body.extend_from_slice(&chunk_frame(
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
    ));
    body.extend_from_slice(&chunk_frame(r#"{"type":"message_stop"}"#));

    Mock::given(method("POST"))
        .and(path(
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/invoke-with-response-stream",
        ))
        .and(header_exists("authorization"))
        .and(header_exists("x-amz-date"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body, "application/vnd.amazon.eventstream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let req = CompletionRequest::new(
        vec![ChatMessage::user("ping")],
        ModelId::new("anthropic.claude-3-5-sonnet-20241022-v2:0"),
        32,
    );
    let events: Vec<StreamEvent> = provider
        .stream(req)
        .await
        .expect("the streaming handshake succeeds")
        .map(|r| r.expect("no stream error"))
        .collect()
        .await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ContentDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "pong");
    assert!(matches!(
        events.iter().find(|e| matches!(e, StreamEvent::Usage(_))),
        Some(StreamEvent::Usage(u)) if u.tokens_in == 9 && u.tokens_out == 2
    ));
    assert!(matches!(
        events.last(),
        Some(StreamEvent::Finish(FinishReason::Stop))
    ));
}
