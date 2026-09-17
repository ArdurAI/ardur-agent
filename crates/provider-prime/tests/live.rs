//! Live round-trip against the REAL `prime-agent` binary.
//!
//! Ignored by default: it spends a real model call and needs a configured
//! prime-agent install. Run explicitly with
//! `cargo test -p ardur-provider-prime --test live -- --ignored --nocapture`.

use std::path::PathBuf;
use std::time::Duration;

use ardur_provider_prime::{PrimeConfig, PrimeProvider};
use ardur_provider_runtime::{CompletionRequest, CostEnvelope, ModelId, Provider, RequestId};
use ardur_runtime::{ChatMessage, Role};

/// Where the installed binary lives, overridable for a different install.
fn binary() -> PathBuf {
    std::env::var("PRIME_AGENT_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME");
            PathBuf::from(home).join(".local/share/prime-agent/bin/prime-agent")
        })
}

#[tokio::test]
#[ignore = "spends a real model call; needs a configured prime-agent install"]
async fn live_round_trip_returns_the_requested_token() {
    let provider = PrimeProvider::new(PrimeConfig {
        binary: binary(),
        provider: Some("kimi-coding".into()),
        default_model: Some("kimi-for-coding-highspeed".into()),
        request_timeout: Duration::from_secs(180),
        ..PrimeConfig::default()
    });

    let req = CompletionRequest {
        request_id: RequestId::new(),
        messages: vec![ChatMessage {
            role: Role::User,
            content: "Reply with exactly: ARDUR-PRIME-LIVE-OK".to_string(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }],
        model: ModelId(String::new()),
        max_tokens: 64,
        temperature: 0.0,
        stop_sequences: Vec::new(),
        requested_cost_envelope: CostEnvelope::default(),
        tools: Vec::new(),
        stream: false,
    };

    let response = provider
        .complete(req)
        .await
        .expect("live turn should succeed");

    println!("live content: {:?}", response.content);
    println!(
        "live usage: in={} out={} cents={}",
        response.usage.tokens_in, response.usage.tokens_out, response.cost.cents
    );

    assert!(
        response.content.contains("ARDUR-PRIME-LIVE-OK"),
        "expected the requested token in the reply, got: {:?}",
        response.content
    );
    // Delegated billing stays zero regardless of what the child spent upstream.
    assert_eq!(response.cost.cents, 0);
    // A real turn must report real token counts, not the zeroed default.
    assert!(
        response.usage.tokens_in > 0,
        "a live turn should report input tokens"
    );
}
