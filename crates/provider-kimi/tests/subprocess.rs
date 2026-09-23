//! Subprocess round-trip tests against a shim standing in for the `kimi` binary.
//!
//! Each test writes a small executable that speaks the real
//! `--print --output-format stream-json` protocol — reading the prompt on
//! stdin and emitting JSONL message objects — so the spawn, stdin feed,
//! deny-by-default agent staging, fail-closed mapping, and timeout are all
//! exercised with no kimi install and no model spend.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ardur_provider_kimi::{KimiConfig, KimiProvider};
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

/// A shim that replays a well-formed stream-json turn: a tool-role line, an
/// intermediate assistant message, then the final assistant message.
const HAPPY_SHIM: &str = r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
emit({"role": "assistant", "content": [{"type": "text", "text": "working on it"}]})
emit({"role": "tool", "content": [{"type": "text", "text": "tool output never surfaces"}]})
emit({"role": "assistant", "content": [
    {"type": "think", "think": "wrapping up"},
    {"type": "text", "text": "SHIM-ROUNDTRIP-OK"}]})
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

fn provider_for(binary: PathBuf) -> KimiProvider {
    KimiProvider::new(KimiConfig {
        binary,
        request_timeout: Duration::from_secs(30),
        ..KimiConfig::default()
    })
}

#[tokio::test]
async fn happy_turn_returns_text_zeroed_usage_and_retained_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(dir.path(), "kimi-happy", HAPPY_SHIM);

    let response = provider_for(shim)
        .complete(request("say the magic words"))
        .await
        .expect("turn should succeed");

    assert_eq!(response.content, "SHIM-ROUNDTRIP-OK");
    // The stream-json vocabulary carries no token usage: counts stay zero
    // rather than being invented.
    assert_eq!(response.usage.tokens_in, 0);
    assert_eq!(response.usage.tokens_out, 0);
    assert_eq!(response.cost.cents, 0);
    assert_eq!(
        response.finish_reason,
        ardur_provider_runtime::FinishReason::Stop
    );
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
    let provider = provider_for(PathBuf::from("/nonexistent/kimi-does-not-exist"));
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
async fn expired_login_on_stdout_maps_to_unauthorized() {
    // The exact failure shape observed from the real CLI (1.49.0): the error
    // line is plain text on *stdout*, stderr carries only session hints.
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "kimi-unauth",
        r#"#!/usr/bin/env python3
import sys
sys.stdin.read()
sys.stdout.write("Error code: 401 - {'error': {'message': 'The API Key appears to be invalid or may have expired. Please verify your credentials and try again.', 'type': 'invalid_authentication_error'}}\n")
sys.stderr.write("\nTo resume this session: kimi -r 8b835118-0000-0000-0000-000000000000\n")
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
        "kimi-crash",
        r#"#!/usr/bin/env python3
import sys
sys.stdin.read()
sys.stderr.write("kimi exploded mid-turn\n")
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
        "kimi-empty",
        r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
sys.stdout.write(json.dumps({
    "role": "assistant",
    "content": [{"type": "text", "text": "   "}],
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
        "kimi-hang",
        r#"#!/usr/bin/env python3
import sys, time
sys.stdin.read()
time.sleep(30)
"#,
    );

    let provider = KimiProvider::new(KimiConfig {
        binary: shim,
        request_timeout: Duration::from_secs(2),
        ..KimiConfig::default()
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
        "kimi-noisy",
        r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
sys.stdout.write("To resume this session: kimi -r abc123\n")
sys.stdout.write(json.dumps({
    "role": "assistant",
    "content": [{"type": "text", "text": "SURVIVED-THE-NOISE"}],
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
async fn retryable_exit_maps_to_a_transient_error() {
    // Print mode exits 75 (EX_TEMPFAIL) for retryable provider conditions
    // (connection/timeout/5xx); the detail line rides stdout as plain text.
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "kimi-retryable",
        r#"#!/usr/bin/env python3
import sys
sys.stdin.read()
sys.stdout.write("Error code: 503 - {'error': {'message': 'service unavailable'}}\n")
sys.exit(75)
"#,
    );

    let err = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect_err("a retryable condition must still fail the turn");
    assert!(
        matches!(err, ProviderError::NetworkFailure(_)),
        "expected NetworkFailure for exit 75, got {err:?}"
    );
}

#[tokio::test]
async fn quota_exhaustion_on_a_retryable_exit_maps_to_rate_limited() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "kimi-ratelimit",
        r#"#!/usr/bin/env python3
import sys
sys.stdin.read()
sys.stdout.write("Error code: 429 - {'error': {'message': 'rate limit exceeded for this API key'}}\n")
sys.exit(75)
"#,
    );

    let err = provider_for(shim)
        .complete(request("hello"))
        .await
        .expect_err("quota exhaustion must fail the turn");
    assert!(
        matches!(err, ProviderError::RateLimited { .. }),
        "expected RateLimited, got {err:?}"
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
async fn deny_agent_spec_is_staged_and_pointed_at_by_default() {
    // The shim records argv and copies the staged agent spec out from under
    // the provider; assert the deny-by-default spec was staged, passed, and
    // cleaned up after the turn.
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "kimi-args",
        r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
argv = sys.argv[1:]
open(sys.argv[0] + ".argv", "w").write("\n".join(argv))
if "--agent-file" in argv:
    spec_path = argv[argv.index("--agent-file") + 1]
    open(sys.argv[0] + ".specpath", "w").write(spec_path)
    open(sys.argv[0] + ".spec", "w").write(open(spec_path).read())
sys.stdout.write(json.dumps({
    "role": "assistant",
    "content": [{"type": "text", "text": "ARGS-OK"}],
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
    assert!(lines.contains(&"--print"), "expected print mode:\n{argv}");
    assert!(
        lines
            .windows(2)
            .any(|w| w == ["--output-format", "stream-json"]),
        "expected --output-format stream-json:\n{argv}"
    );
    for forbidden in ["--yolo", "--yes", "-y", "--afk"] {
        assert!(
            !lines.contains(&forbidden),
            "{forbidden} must never be passed by default:\n{argv}"
        );
    }

    let spec_path =
        std::fs::read_to_string(format!("{}.specpath", shim.display())).expect("spec path file");
    assert!(
        spec_path.contains("ardur-kimi-deny-agent-"),
        "expected a staged deny-spec path, got: {spec_path}"
    );
    let spec = std::fs::read_to_string(format!("{}.spec", shim.display())).expect("spec copy");
    assert!(
        spec.contains("extend: default"),
        "spec must extend the builtin default agent: {spec}"
    );
    assert!(
        spec.contains("tools: []"),
        "spec must resolve to an empty tool list: {spec}"
    );
    assert!(
        spec.contains("subagents: {}"),
        "spec must drop the default subagents: {spec}"
    );

    // The staged file is per-turn state: once the child exits it is removed.
    assert!(
        !Path::new(spec_path.trim()).exists(),
        "staged deny spec must be removed after the turn: {spec_path}"
    );
}

#[tokio::test]
async fn allowing_child_tools_omits_the_agent_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "kimi-tools-env",
        r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
open(sys.argv[0] + ".argv", "w").write("\n".join(sys.argv[1:]))
sys.stdout.write(json.dumps({
    "role": "assistant",
    "content": [{"type": "text", "text": "TOOLS-ENV-OK"}],
}) + "\n")
sys.exit(0)
"#,
    );

    let provider = KimiProvider::new(KimiConfig {
        binary: shim.clone(),
        allow_child_tools: true,
        request_timeout: Duration::from_secs(30),
        ..KimiConfig::default()
    });
    let response = provider
        .complete(request("hello"))
        .await
        .expect("turn should succeed");
    assert_eq!(response.content, "TOOLS-ENV-OK");

    let argv = std::fs::read_to_string(format!("{}.argv", shim.display())).expect("argv file");
    assert!(
        !argv.lines().any(|l| l == "--agent-file"),
        "opting into child tools must use the operator's own default agent:\n{argv}"
    );
}

#[tokio::test]
async fn chosen_model_is_passed_as_dash_m() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shim = write_shim(
        dir.path(),
        "kimi-model",
        r#"#!/usr/bin/env python3
import json, sys
sys.stdin.read()
open(sys.argv[0] + ".argv", "w").write("\n".join(sys.argv[1:]))
sys.stdout.write(json.dumps({
    "role": "assistant",
    "content": [{"type": "text", "text": "MODEL-OK"}],
}) + "\n")
sys.exit(0)
"#,
    );

    let provider = KimiProvider::new(KimiConfig {
        binary: shim.clone(),
        default_model: Some("kimi-code/kimi-for-coding".into()),
        request_timeout: Duration::from_secs(30),
        ..KimiConfig::default()
    });
    provider
        .complete(request("hello"))
        .await
        .expect("turn should succeed");

    let argv = std::fs::read_to_string(format!("{}.argv", shim.display())).expect("argv file");
    let lines: Vec<&str> = argv.lines().collect();
    assert!(
        lines
            .windows(2)
            .any(|w| w == ["-m", "kimi-code/kimi-for-coding"]),
        "expected -m <default model>:\n{argv}"
    );
}

#[tokio::test]
async fn an_unenforceable_output_ceiling_is_refused_not_ignored() {
    // kimi --print has no per-completion output cap, so a ceiling below the
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
    let shim = write_shim(dir.path(), "kimi-floor-ok", HAPPY_SHIM);
    let provider = KimiProvider::new(KimiConfig {
        binary: shim,
        max_tokens_floor: 0,
        request_timeout: Duration::from_secs(30),
        ..KimiConfig::default()
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
    let shim = write_shim(dir.path(), "kimi-zero-ceiling", HAPPY_SHIM);
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
