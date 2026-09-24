//! Subprocess round-trip tests against a shim standing in for the `opencode` binary.
//!
//! Each test writes a small executable that speaks the real `run --format json`
//! protocol — reading the prompt on stdin and emitting JSONL events — so the
//! spawn, stdin feed, inline-config posture, fail-closed mapping, and timeout
//! are all exercised with no opencode install and no model spend.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ardur_provider_opencode::{OpenCodeConfig, OpenCodeProvider};
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

/// A shim that replays a well-formed `run --format json` turn.
const HAPPY_SHIM: &str = r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
emit({"type": "step_start", "timestamp": 1, "sessionID": "s1", "part": {"type": "step-start"}})
emit({"type": "text", "timestamp": 2, "sessionID": "s1",
      "part": {"type": "text", "text": "SHIM-ROUNDTRIP-OK", "time": {"start": 1, "end": 2}}})
emit({"type": "step_finish", "timestamp": 3, "sessionID": "s1",
      "part": {"type": "step-finish", "reason": "stop", "cost": 0,
               "tokens": {"input": 41, "output": 7, "reasoning": 0,
                          "cache": {"read": 0, "write": 0}}}})
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
        // At/above the default `max_tokens_floor`: this backend refuses a
        // ceiling it cannot enforce, so the shared fixture must ask for one it
        // can honour. The refusal path has its own dedicated test.
        8_192,
    )
}

fn provider_for(binary: PathBuf) -> OpenCodeProvider {
    OpenCodeProvider::new(OpenCodeConfig {
        binary,
        request_timeout: Duration::from_secs(30),
        ..OpenCodeConfig::default()
    })
}

#[tokio::test]
async fn happy_turn_returns_text_usage_and_retained_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(dir.path(), "opencode-happy", HAPPY_SHIM);

    let response = provider_for(shim)
        .complete(request("say the magic words"))
        .await
        .expect("turn should succeed");

    assert_eq!(response.content, "SHIM-ROUNDTRIP-OK");
    assert_eq!(response.usage.tokens_in, 41);
    assert_eq!(response.usage.tokens_out, 7);
    assert_eq!(response.cost.cents, 0);
    assert_eq!(response.cost.tokens_in, 41);
    let raw = response.raw_provider_response.expect("raw retained");
    let obj = raw.as_object().expect("object with events");
    let events = obj
        .get("events")
        .and_then(|v| v.as_array())
        .expect("events array")
        .len();
    assert!(events >= 2, "expected retained events, got {events}");
}

#[tokio::test]
async fn missing_binary_is_reported_as_a_missing_install() {
    let provider = provider_for(PathBuf::from("/nonexistent/opencode-does-not-exist"));
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
            // #571: the failure must be actionable without opening the docs,
            // and smoke must be able to tell "not installed" apart from
            // auth / rate-limit / general child failures.
            assert!(
                msg.contains("curl -fsSL https://opencode.ai/install | bash"),
                "install one-liner must ride in the message, got: {msg}"
            );
            assert!(
                msg.contains("/nonexistent/opencode-does-not-exist"),
                "attempted binary must be named, got: {msg}"
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
        "opencode-unauth",
        r#"#!/usr/bin/env python3
import sys
sys.stdin.read()
sys.stderr.write("ProviderAuthError: not logged in; run opencode auth login\n")
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
        "opencode-crash",
        r#"#!/usr/bin/env python3
import sys
sys.stdin.read()
sys.stderr.write("opencode exploded mid-turn\n")
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
        "opencode-empty",
        r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
sys.stdout.write(json.dumps({
    "type": "text",
    "sessionID": "s1",
    "part": {"type": "text", "text": "   "},
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
        "opencode-hang",
        r#"#!/usr/bin/env python3
import sys, time
sys.stdin.read()
time.sleep(30)
"#,
    );

    let provider = OpenCodeProvider::new(OpenCodeConfig {
        binary: shim,
        request_timeout: Duration::from_secs(2),
        ..OpenCodeConfig::default()
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
        "opencode-noisy",
        r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
sys.stdout.write("Model scope: anthropic (warming)\n")
sys.stdout.write(json.dumps({
    "type": "text",
    "sessionID": "s1",
    "part": {"type": "text", "text": "SURVIVED-THE-NOISE"},
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
async fn in_band_error_event_fails_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "opencode-result-err",
        r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
sys.stdout.write(json.dumps({
    "type": "error",
    "sessionID": "s1",
    "error": {"name": "APIError", "data": {"message": "provider rejected the request"}},
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
async fn an_in_band_auth_error_event_maps_to_unauthorized() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "opencode-auth-event",
        r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
sys.stdout.write(json.dumps({
    "type": "error",
    "sessionID": "s1",
    "error": {"name": "ProviderAuthError", "data": {"message": "ProviderAuthError: invalid api key"}},
}) + "\n")
sys.exit(0)
"#,
    );

    let err = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect_err("in-band auth error must fail the turn");
    assert!(
        matches!(err, ProviderError::Unauthorized),
        "expected Unauthorized, got {err:?}"
    );
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
async fn deny_all_inline_config_is_set_by_default() {
    // The shim records argv and the inline-config env var; assert the deny-all
    // inline config was handed to the child and no tool-enabling flag was.
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "opencode-args",
        r#"#!/usr/bin/env python3
import json, os, sys
sys.stdin.read()
open(sys.argv[0] + ".argv", "w").write("\n".join(sys.argv[1:]))
open(sys.argv[0] + ".env", "w").write(os.environ.get("OPENCODE_CONFIG_CONTENT", ""))
sys.stdout.write(json.dumps({
    "type": "text",
    "sessionID": "s1",
    "part": {"type": "text", "text": "ARGS-OK"},
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
        lines.contains(&"run"),
        "expected the run subcommand:\n{argv}"
    );
    assert!(
        lines.windows(2).any(|w| w == ["--format", "json"]),
        "expected --format json:\n{argv}"
    );
    for forbidden in ["--auto", "--share"] {
        assert!(
            !lines.contains(&forbidden),
            "{forbidden} must never be passed by default:\n{argv}"
        );
    }

    let inline =
        std::fs::read_to_string(format!("{}.env", shim.display())).expect("inline config file");
    let parsed: serde_json::Value =
        serde_json::from_str(&inline).expect("inline config is valid JSON");
    assert_eq!(
        parsed["permission"], "deny",
        "child tools must be denied by default: {inline}"
    );
    assert_eq!(
        parsed["share"], "disabled",
        "session sharing must be pinned off: {inline}"
    );
}

#[tokio::test]
async fn allowing_child_tools_keeps_operator_permissions_but_pins_sharing_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "opencode-tools-env",
        r#"#!/usr/bin/env python3
import json, os, sys
sys.stdin.read()
open(sys.argv[0] + ".env", "w").write(os.environ.get("OPENCODE_CONFIG_CONTENT", ""))
sys.stdout.write(json.dumps({
    "type": "text",
    "sessionID": "s1",
    "part": {"type": "text", "text": "TOOLS-ENV-OK"},
}) + "\n")
sys.exit(0)
"#,
    );

    let provider = OpenCodeProvider::new(OpenCodeConfig {
        binary: shim.clone(),
        allow_child_tools: true,
        request_timeout: Duration::from_secs(30),
        ..OpenCodeConfig::default()
    });
    let response = provider
        .complete(request("hello"))
        .await
        .expect("turn should succeed");
    assert_eq!(response.content, "TOOLS-ENV-OK");

    let inline =
        std::fs::read_to_string(format!("{}.env", shim.display())).expect("inline config file");
    let parsed: serde_json::Value =
        serde_json::from_str(&inline).expect("inline config is valid JSON");
    assert!(
        parsed.get("permission").is_none(),
        "opting into child tools must not override operator permissions: {inline}"
    );
    assert_eq!(
        parsed["share"], "disabled",
        "session sharing stays pinned off in both modes: {inline}"
    );
}

#[tokio::test]
async fn an_unenforceable_output_ceiling_is_refused_not_ignored() {
    // opencode run has no per-completion output cap, so a ceiling below the
    // floor must fail rather than be silently discarded — otherwise the
    // runtime authorizes N tokens and is billed for more while the response
    // still reports a clean stop.
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
    let shim = write_shim(dir.path(), "opencode-floor-ok", HAPPY_SHIM);
    let provider = OpenCodeProvider::new(OpenCodeConfig {
        binary: shim,
        max_tokens_floor: 0,
        request_timeout: Duration::from_secs(30),
        ..OpenCodeConfig::default()
    });

    let mut req = request("hello");
    req.max_tokens = 16;
    let response = provider
        .complete(req)
        .await
        .expect("zero floor must accept any ceiling");
    assert_eq!(response.content, "SHIM-ROUNDTRIP-OK");
}

#[tokio::test]
async fn a_zero_max_tokens_delegates_regardless_of_floor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(dir.path(), "opencode-zero-ceiling", HAPPY_SHIM);
    let response = provider_for(shim)
        .complete({
            let mut req = request("hello");
            req.max_tokens = 0;
            req
        })
        .await
        .expect("max_tokens=0 must delegate");
    assert_eq!(response.content, "SHIM-ROUNDTRIP-OK");
}
