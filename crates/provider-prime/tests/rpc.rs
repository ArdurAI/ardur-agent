//! RPC round-trip tests against a shim standing in for the `prime-agent` binary.
//!
//! Each test writes a small executable that speaks the real protocol — reading
//! newline-delimited commands on stdin and replying with `response` lines and an
//! event stream — so the spawn, handshake, event drain, and failure mapping are
//! all exercised with no prime-agent install and no model spend.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ardur_provider_prime::{PrimeConfig, PrimeProvider};
use ardur_provider_runtime::{
    CompletionRequest, CostEnvelope, ModelId, Provider, ProviderError, RequestId,
};
use ardur_runtime::{ChatMessage, Role};

/// Write an executable shim script and return its path.
fn write_shim(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut file = std::fs::File::create(&path).expect("create shim");
    file.write_all(body.as_bytes()).expect("write shim");
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod shim");
    }
    path
}

/// A shim that replays a well-formed turn: prompt ack, a couple of events with
/// usage, `agent_end`, then the final text.
const HAPPY_SHIM: &str = r#"#!/usr/bin/env python3
import json, sys

def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    cmd = json.loads(line)
    kind = cmd.get("type")
    if kind == "prompt":
        emit({"id": cmd.get("id"), "type": "response", "command": "prompt", "success": True})
        emit({"type": "agent_start"})
        emit({"type": "message_end", "message": {"usage": {"input": 41, "output": 7}}})
        emit({"type": "agent_end"})
    elif kind == "get_last_assistant_text":
        emit({
            "id": cmd.get("id"),
            "type": "response",
            "command": "get_last_assistant_text",
            "success": True,
            "data": {"text": "SHIM-ROUNDTRIP-OK"},
        })
        break
"#;

fn request(prompt: &str) -> CompletionRequest {
    CompletionRequest {
        request_id: RequestId::new(),
        messages: vec![ChatMessage {
            role: Role::User,
            content: prompt.to_string(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }],
        model: ModelId(String::new()),
        max_tokens: 256,
        temperature: 0.0,
        stop_sequences: Vec::new(),
        requested_cost_envelope: CostEnvelope::default(),
        tools: Vec::new(),
        stream: false,
    }
}

fn provider_for(binary: PathBuf) -> PrimeProvider {
    PrimeProvider::new(PrimeConfig {
        binary,
        request_timeout: Duration::from_secs(30),
        ..PrimeConfig::default()
    })
}

#[tokio::test]
async fn happy_turn_returns_text_usage_and_retained_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(dir.path(), "prime-happy", HAPPY_SHIM);

    let response = provider_for(shim)
        .complete(request("say the magic words"))
        .await
        .expect("turn should succeed");

    assert_eq!(response.content, "SHIM-ROUNDTRIP-OK");
    // Usage must come from the child's report, not be invented.
    assert_eq!(response.usage.tokens_in, 41);
    assert_eq!(response.usage.tokens_out, 7);
    // Delegated billing: prime-agent's own provider pays, so zero cents here.
    assert_eq!(response.cost.cents, 0);
    assert_eq!(response.cost.tokens_in, 41);
    // The event stream is retained as the audit body.
    let events = response
        .raw_provider_response
        .expect("events retained")
        .as_array()
        .expect("array")
        .len();
    assert!(events >= 2, "expected retained events, got {events}");
}

#[tokio::test]
async fn missing_binary_is_reported_as_a_missing_install() {
    let provider = provider_for(PathBuf::from("/nonexistent/prime-agent-does-not-exist"));
    let err = provider
        .complete(request("hello"))
        .await
        .expect_err("missing binary must fail");
    match err {
        ProviderError::Upstream(msg) => {
            assert!(
                msg.contains("not installed"),
                "expected an install diagnostic, got: {msg}"
            );
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn login_failure_maps_to_unauthorized_not_a_generic_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "prime-unauth",
        r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    cmd = json.loads(line)
    sys.stdout.write(json.dumps({
        "id": cmd.get("id"), "type": "response", "command": cmd.get("type"),
        "success": False, "error": "not logged in; run /login",
    }) + "\n")
    sys.stdout.flush()
    break
"#,
    );

    let err = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect_err("auth failure must fail");
    assert!(
        matches!(err, ProviderError::Unauthorized),
        "expected Unauthorized, got {err:?}"
    );
}

#[tokio::test]
async fn child_exiting_before_agent_end_fails_closed_with_stderr() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "prime-crash",
        r#"#!/usr/bin/env python3
import json, sys
line = sys.stdin.readline()
cmd = json.loads(line)
sys.stdout.write(json.dumps({
    "id": cmd.get("id"), "type": "response", "command": "prompt", "success": True,
}) + "\n")
sys.stdout.flush()
sys.stderr.write("prime-agent exploded mid-turn\n")
sys.exit(3)
"#,
    );

    let err = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect_err("a crashed turn must not look successful");
    match err {
        ProviderError::Upstream(msg) => {
            // The turn must not be reported as complete, and the child's own
            // diagnostic must survive into the error.
            assert!(
                msg.contains("before the turn completed"),
                "expected a turn-incomplete diagnostic, got: {msg}"
            );
            assert!(
                msg.contains("exploded"),
                "expected captured stderr in the error, got: {msg}"
            );
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn empty_assistant_text_is_a_failure_not_an_empty_success() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "prime-empty",
        r#"#!/usr/bin/env python3
import json, sys

def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    cmd = json.loads(line)
    if cmd.get("type") == "prompt":
        emit({"id": cmd.get("id"), "type": "response", "command": "prompt", "success": True})
        emit({"type": "agent_end"})
    else:
        emit({"id": cmd.get("id"), "type": "response", "command": "get_last_assistant_text",
              "success": True, "data": {"text": "   "}})
        break
"#,
    );

    let err = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect_err("an empty turn must not be a success");
    match err {
        ProviderError::Upstream(msg) => assert!(
            msg.contains("no assistant text"),
            "expected an empty-output diagnostic, got: {msg}"
        ),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn a_hanging_child_is_bounded_by_the_request_timeout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "prime-hang",
        r#"#!/usr/bin/env python3
import sys, time
sys.stdin.readline()
# Well above the 2s test timeout, but short enough that a *mutated* build
# (timeout removed) fails in seconds rather than stalling a proof run.
time.sleep(30)
"#,
    );

    let provider = PrimeProvider::new(PrimeConfig {
        binary: shim,
        request_timeout: Duration::from_secs(2),
        ..PrimeConfig::default()
    });

    let started = std::time::Instant::now();
    let err = provider
        .complete(request("hello"))
        .await
        .expect_err("a hanging child must time out");
    assert!(
        matches!(err, ProviderError::NetworkFailure(_)),
        "expected NetworkFailure, got {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "timeout did not bound the turn"
    );
}

#[tokio::test]
async fn non_json_stdout_noise_does_not_abort_a_valid_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "prime-noisy",
        r#"#!/usr/bin/env python3
import json, sys

def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

sys.stdout.write("Model scope: kimi-coding/k3 (Alt+M to cycle)\n")
sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    cmd = json.loads(line)
    if cmd.get("type") == "prompt":
        emit({"id": cmd.get("id"), "type": "response", "command": "prompt", "success": True})
        sys.stdout.write("still warming up\n")
        sys.stdout.flush()
        emit({"type": "agent_end"})
    else:
        emit({"id": cmd.get("id"), "type": "response", "command": "get_last_assistant_text",
              "success": True, "data": {"text": "SURVIVED-THE-NOISE"}})
        break
"#,
    );

    let response = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect("human-facing stdout noise must not abort the turn");
    assert_eq!(response.content, "SURVIVED-THE-NOISE");
}

#[tokio::test]
async fn empty_prompt_is_rejected_before_spawning_a_child() {
    // A binary that would fail loudly if it were ever spawned.
    let provider = provider_for(PathBuf::from("/nonexistent/must-not-spawn"));
    let mut req = request("");
    req.messages.clear();

    let err = provider
        .complete(req)
        .await
        .expect_err("an empty prompt must be rejected");
    assert!(
        matches!(err, ProviderError::InvalidRequest(_)),
        "expected InvalidRequest (not a spawn failure), got {err:?}"
    );
}
