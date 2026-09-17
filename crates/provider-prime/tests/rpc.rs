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
        // At/above the default `max_tokens_floor`: this backend refuses a
        // ceiling it cannot enforce, so the shared fixture must ask for one it
        // can honour. The refusal path has its own dedicated test.
        max_tokens: 8_192,
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
async fn agent_end_without_a_prompt_ack_is_not_a_completed_turn() {
    // Review finding (fail-closed hole): a child that emits `agent_end` without
    // ever acknowledging the prompt must not be treated as a completed turn,
    // even when the session still holds last-assistant text from earlier.
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "prime-no-ack",
        r#"#!/usr/bin/env python3
import json, sys

def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

# Read the prompt, then emit `agent_end` with NO prompt acknowledgement and
# exit. Exiting matters: a correct provider fails at the missing ack and never
# sends a follow-up command, so a shim that kept reading stdin would idle until
# the turn timeout and mask the assertion under a NetworkFailure.
sys.stdin.readline()
emit({"type": "agent_end"})
"#,
    );

    let err = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect_err("an unacknowledged prompt must not look successful");
    match err {
        ProviderError::Upstream(msg) => assert!(
            msg.contains("without acknowledging the prompt"),
            "expected a missing-ack diagnostic, got: {msg}"
        ),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn a_rejected_prompt_is_not_rescued_by_stale_assistant_text() {
    // Same hole from the other side: the prompt is explicitly rejected, yet the
    // session would still answer `get_last_assistant_text`.
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "prime-rejected",
        r#"#!/usr/bin/env python3
import json, sys

def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

cmd = json.loads(sys.stdin.readline())
emit({"id": cmd.get("id"), "type": "response", "command": "prompt",
      "success": False, "error": "prompt rejected by policy"})
emit({"type": "agent_end"})
"#,
    );

    let err = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect_err("a rejected prompt must fail the turn");
    match err {
        ProviderError::Upstream(msg) => assert!(
            msg.contains("rejected") || msg.contains("acknowledging"),
            "expected a rejection diagnostic, got: {msg}"
        ),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unterminated_stdout_flood_is_bounded_not_buffered_forever() {
    // Review finding: `read_line` grows its buffer until a newline or EOF, so a
    // child emitting a newline-free flood could exhaust memory before any
    // post-hoc length check ran. The read itself must be bounded.
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "prime-flood",
        r#"#!/usr/bin/env python3
import sys
sys.stdin.readline()
# 64 MiB with no newline, well past the 8 MiB line cap.
chunk = "A" * (1 << 20)
for _ in range(64):
    sys.stdout.write(chunk)
    sys.stdout.flush()
"#,
    );

    let started = std::time::Instant::now();
    let err = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect_err("an oversized line must be refused");
    match err {
        ProviderError::Upstream(msg) => assert!(
            msg.contains("oversized"),
            "expected an oversized-line diagnostic, got: {msg}"
        ),
        other => panic!("expected Upstream, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the bounded read should fail fast"
    );
}

#[tokio::test]
async fn usage_accumulates_over_a_multi_message_turn() {
    // Review finding: several assistant messages in one turn must sum, not
    // overwrite.
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "prime-multi",
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
        emit({"type": "message_end", "message": {"usage": {"input": 100, "output": 10}}})
        emit({"type": "message_end", "message": {"usage": {"input": 50, "output": 5}}})
        emit({"type": "agent_end"})
    else:
        emit({"id": cmd.get("id"), "type": "response",
              "command": "get_last_assistant_text", "success": True,
              "data": {"text": "MULTI-OK"}})
        break
"#,
    );

    let response = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect("turn should succeed");
    assert_eq!(
        response.usage.tokens_in, 150,
        "input tokens across messages must sum, not overwrite"
    );
    assert_eq!(response.usage.tokens_out, 15);
    assert_eq!(response.cost.tokens_in, 150);
}

#[tokio::test]
async fn an_unenforceable_output_ceiling_is_refused_not_ignored() {
    // Review finding: prime-agent has no per-completion output cap, so a
    // ceiling below the floor must fail rather than be silently discarded —
    // otherwise the runtime authorizes N tokens and is billed for more while
    // the response still reports a clean stop.
    let provider = provider_for(PathBuf::from("/nonexistent/must-not-spawn"));
    let mut req = request("hello");
    req.max_tokens = 16;

    let err = provider
        .complete(req)
        .await
        .expect_err("an unenforceable ceiling must be refused");
    match err {
        ProviderError::InvalidRequest(msg) => assert!(
            msg.contains("cannot enforce"),
            "expected an enforcement diagnostic, got: {msg}"
        ),
        other => panic!("expected InvalidRequest (not a spawn failure), got {other:?}"),
    }
}

#[tokio::test]
async fn a_zero_floor_delegates_enforcement_and_accepts_any_ceiling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(dir.path(), "prime-floor-ok", HAPPY_SHIM);
    let provider = PrimeProvider::new(PrimeConfig {
        binary: shim,
        max_tokens_floor: 0,
        request_timeout: Duration::from_secs(30),
        ..PrimeConfig::default()
    });

    let mut req = request("hello");
    req.max_tokens = 16;
    let response = provider
        .complete(req)
        .await
        .expect("a zero floor delegates enforcement");
    assert_eq!(response.content, "SHIM-ROUNDTRIP-OK");
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
