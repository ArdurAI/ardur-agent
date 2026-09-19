//! Subprocess round-trip tests against a shim standing in for the `hermes` binary.
//!
//! Each test writes a small executable that speaks the real stream-json protocol
//! — reading the prompt on stdin and emitting JSONL events including a terminal
//! `result` — so the spawn, stdin feed, fail-closed mapping, and timeout are
//! all exercised with no hermes install and no model spend.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ardur_provider_hermes::{HermesConfig, HermesProvider};
use ardur_provider_runtime::{CompletionRequest, ModelId, Provider, ProviderError};
use ardur_runtime::{ChatMessage, Role};

/// Write an executable shim script and return its path.
fn write_shim(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut file = std::fs::File::create(&path).expect("create shim");
    file.write_all(body.as_bytes()).expect("write shim");
    drop(file);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod shim");
    path
}

/// A shim that replays a well-formed stream-json turn.
const HAPPY_SHIM: &str = r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
emit({"type": "system", "subtype": "init", "model": "shim", "session_id": "s1"})
emit({"type": "text", "text": "SHIM"})
emit({
    "type": "result",
    "session_id": "s1",
    "exit_code": 0,
    "text": "SHIM-ROUNDTRIP-OK",
    "tokens": {"input": 41, "output": 7, "total": 48},
})
sys.exit(0)
"#;

fn request(prompt: &str) -> CompletionRequest {
    CompletionRequest::new(
        vec![ChatMessage {
            role: Role::User,
            content: prompt.to_string(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }],
        ModelId(String::new()),
        8_192,
    )
}

fn provider_for(binary: PathBuf) -> HermesProvider {
    HermesProvider::new(HermesConfig {
        binary,
        request_timeout: Duration::from_secs(30),
        ..HermesConfig::default()
    })
}

#[tokio::test]
async fn happy_turn_returns_text_usage_and_retained_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(dir.path(), "hermes-happy", HAPPY_SHIM);

    let response = provider_for(shim)
        .complete(request("say the magic words"))
        .await
        .expect("turn should succeed");

    assert_eq!(response.content, "SHIM-ROUNDTRIP-OK");
    assert_eq!(response.usage.tokens_in, 41);
    assert_eq!(response.usage.tokens_out, 7);
    assert_eq!(response.cost.cents, 0);
    assert_eq!(response.cost.tokens_in, 41);
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
    let provider = provider_for(PathBuf::from("/nonexistent/hermes-agent-does-not-exist"));
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
        "hermes-unauth",
        r#"#!/usr/bin/env python3
import sys
sys.stdin.read()
sys.stderr.write("not logged in; run hermes auth\n")
sys.exit(1)
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
async fn child_exiting_nonzero_fails_closed_with_stderr() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "hermes-crash",
        r#"#!/usr/bin/env python3
import sys
sys.stdin.read()
sys.stderr.write("hermes exploded mid-turn\n")
sys.exit(3)
"#,
    );

    let err = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect_err("a crashed turn must not look successful");
    match err {
        ProviderError::Upstream(msg) => {
            assert!(
                msg.contains("exploded") || msg.contains("status 3"),
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
        "hermes-empty",
        r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
sys.stdout.write(json.dumps({
    "type": "result",
    "exit_code": 0,
    "text": "   ",
    "tokens": {"input": 1, "output": 0},
}) + "\n")
sys.exit(0)
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
        "hermes-hang",
        r#"#!/usr/bin/env python3
import sys, time
sys.stdin.read()
time.sleep(30)
"#,
    );

    let provider = HermesProvider::new(HermesConfig {
        binary: shim,
        request_timeout: Duration::from_secs(2),
        ..HermesConfig::default()
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
        "hermes-noisy",
        r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
sys.stdout.write("Model scope: openrouter (warming)\n")
sys.stdout.write(json.dumps({
    "type": "result",
    "exit_code": 0,
    "text": "SURVIVED-THE-NOISE",
    "tokens": {"input": 2, "output": 1},
}) + "\n")
sys.exit(0)
"#,
    );

    let response = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect("human-facing stdout noise must not abort the turn");
    assert_eq!(response.content, "SURVIVED-THE-NOISE");
}

#[tokio::test]
async fn in_band_result_error_fails_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "hermes-result-err",
        r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
sys.stdout.write(json.dumps({
    "type": "result",
    "exit_code": 1,
    "text": "",
    "error": "provider rejected the request",
    "tokens": {},
}) + "\n")
sys.exit(1)
"#,
    );

    let err = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect_err("in-band error must fail the turn");
    match err {
        ProviderError::Upstream(msg) => assert!(
            msg.contains("rejected") || msg.contains("status 1"),
            "expected a rejection diagnostic, got: {msg}"
        ),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn empty_prompt_is_rejected_before_spawning_a_child() {
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

#[tokio::test]
async fn deny_all_toolsets_flag_is_passed_by_default() {
    // The shim records argv; assert `--toolsets` with an empty value was
    // present (Hermes deny-all) and `--yolo` was not.
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "hermes-args",
        r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
open(sys.argv[0] + ".argv", "w").write("\n".join(sys.argv[1:]))
sys.stdout.write(json.dumps({
    "type": "result",
    "exit_code": 0,
    "text": "ARGS-OK",
    "tokens": {"input": 1, "output": 1},
}) + "\n")
sys.exit(0)
"#,
    );

    let response = provider_for(shim.clone())
        .complete(request("hello"))
        .await
        .expect("turn should succeed");
    assert_eq!(response.content, "ARGS-OK");

    let argv = std::fs::read_to_string(format!("{}.argv", shim.display())).expect("argv file");
    let lines: Vec<&str> = argv.lines().collect();
    assert!(
        lines.contains(&"--toolsets=") || lines.contains(&"--toolsets"),
        "expected deny-all --toolsets in argv, got:\n{argv}"
    );
    // Prefer the single-arg `--toolsets=` form (a bare empty argv is dropped
    // by execve on Linux and would silently keep Hermes defaults).
    assert!(
        lines.contains(&"--toolsets="),
        "expected --toolsets= (explicit empty), got:\n{argv}"
    );
    assert!(
        !lines.contains(&"--yolo"),
        "--yolo must never be passed by default:\n{argv}"
    );
    assert!(lines.contains(&"--oneshot"), "expected --oneshot:\n{argv}");
    assert!(
        lines.windows(2).any(|w| w == ["--format", "stream-json"]),
        "expected --format stream-json:\n{argv}"
    );
}
