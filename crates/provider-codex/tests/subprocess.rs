//! Subprocess round-trip tests for the Codex provider.
//!
//! These drive [`CodexProvider::complete`] against a tiny executable shim that
//! stands in for the real `codex` binary — emitting known JSONL on success, or
//! failing on purpose — so the spawn + stdin + parse plumbing, the
//! deny-by-default sandbox flags, the max-tokens floor, redaction, and the
//! fail-closed mappings are all exercised with no codex install and no
//! ChatGPT subscription spend. The shim is a POSIX `sh` script, so the suite
//! is `#[cfg(unix)]` (CI runs on macOS/Linux).

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use ardur_provider_codex::{CodexConfig, CodexProvider, SandboxMode};
use ardur_provider_runtime::{CompletionRequest, ModelId, Provider, ProviderError};
use ardur_runtime::ChatMessage;
use tempfile::TempDir;

/// Write `body` as an executable shim into a fresh tempdir, returning the dir
/// (kept alive for the test) and the shim path.
fn write_shim(body: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("codex-shim.sh");
    fs::write(&path, body).expect("write shim");
    let mut perms = fs::metadata(&path).expect("stat shim").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod shim");
    (dir, path)
}

/// A shim that records its argv next to itself and then replies with a known
/// JSONL event stream.
const ARGS_SHIM: &str = "#!/bin/sh\n\
cat > /dev/null\n\
printf '%s\\n' \"$@\" > \"$(dirname \"$0\")/argv.txt\"\n\
printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ARGS-OK\"}}'\n";

fn simple_request() -> CompletionRequest {
    CompletionRequest::new(
        vec![ChatMessage::user("ping")],
        ModelId::new("gpt-5-codex"),
        // At/above the default `max_tokens_floor`: this backend refuses a
        // ceiling it cannot enforce, so the shared fixture must ask for one it
        // can honour. The refusal path has its own dedicated test.
        8_192,
    )
}

#[tokio::test]
async fn binary_not_found_returns_config_error() {
    // A binary that does not exist → the "not installed" Upstream error (the
    // ProviderError taxonomy has no dedicated ConfigError variant; see lib docs).
    let provider = CodexProvider::new(
        CodexConfig::new().codex_binary("/nonexistent/path/to/codex-xyz"),
        ModelId::new("gpt-5-codex"),
    );
    let err = provider.complete(simple_request()).await.unwrap_err();
    match err {
        ProviderError::Upstream(msg) => {
            assert!(
                msg.contains("Codex CLI not installed"),
                "expected not-installed message, got: {msg}"
            );
        }
        other => panic!("expected Upstream not-installed, got {other:?}"),
    }
}

#[tokio::test]
async fn mocked_subprocess_returns_response() {
    // Shim consumes stdin and emits a known JSONL event stream.
    let (dir, shim) = write_shim(
        "#!/bin/sh\n\
         cat > /dev/null\n\
         printf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"t1\"}'\n\
         printf '%s\\n' '{\"type\":\"turn.started\"}'\n\
         printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"agent_message\",\"text\":\"shim-pong\"}}'\n\
         printf '%s\\n' '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}'\n",
    );
    let provider = CodexProvider::new(
        CodexConfig::new()
            .codex_binary(shim)
            .sandbox_mode(SandboxMode::ReadOnly),
        ModelId::new("gpt-5-codex"),
    );

    let resp = provider
        .complete(simple_request())
        .await
        .expect("completion");
    assert_eq!(resp.content, "shim-pong");
    assert_eq!(resp.usage.tokens_in, 12);
    assert_eq!(resp.usage.tokens_out, 3);
    // Subscription-billed: tokens attributed, but zero monetary cost.
    assert_eq!(resp.cost.tokens_in, 12);
    assert_eq!(resp.cost.tokens_out, 3);
    assert_eq!(resp.cost.cents, 0);
    let raw = resp.raw_provider_response.expect("raw retained");
    let obj = raw.as_object().expect("object with events");
    assert!(obj.contains_key("events"), "raw body retains events");
    assert_eq!(
        obj.get("model").and_then(|m| m.as_str()),
        Some("gpt-5-codex"),
        "the chosen model is recorded on the raw body"
    );
    drop(dir);
}

#[tokio::test]
async fn deny_by_default_sandbox_flag_is_passed() {
    // The default sandbox must be the most restrictive codex accepts; the
    // recorded argv proves the child actually ran under it.
    let (dir, shim) = write_shim(ARGS_SHIM);
    let provider = CodexProvider::new(CodexConfig::new().codex_binary(&shim), ModelId::new(""));

    let resp = provider
        .complete(simple_request())
        .await
        .expect("completion");
    assert_eq!(resp.content, "ARGS-OK");

    let argv = fs::read_to_string(dir.path().join("argv.txt")).expect("argv file");
    let lines: Vec<&str> = argv.lines().collect();
    assert!(lines.contains(&"exec"), "expected exec subcommand:\n{argv}");
    assert!(lines.contains(&"--json"), "expected --json:\n{argv}");
    assert!(
        lines.contains(&"--ephemeral"),
        "expected --ephemeral:\n{argv}"
    );
    assert!(
        lines.contains(&"--skip-git-repo-check"),
        "expected --skip-git-repo-check:\n{argv}"
    );
    assert!(
        lines.windows(2).any(|w| w == ["-s", "read-only"]),
        "expected -s read-only (deny-by-default sandbox):\n{argv}"
    );
    assert!(
        lines.windows(2).any(|w| w == ["--color", "never"]),
        "expected --color never:\n{argv}"
    );
    for forbidden in [
        "--dangerously-bypass-approvals-and-sandbox",
        "--approve-for-me",
        "danger-full-access",
        "workspace-write",
    ] {
        assert!(
            !lines.contains(&forbidden),
            "{forbidden} must never be passed by default:\n{argv}"
        );
    }
}

#[tokio::test]
async fn opting_into_a_looser_sandbox_passes_its_flag() {
    let (dir, shim) = write_shim(ARGS_SHIM);
    let provider = CodexProvider::new(
        CodexConfig::new()
            .codex_binary(&shim)
            .sandbox_mode(SandboxMode::WorkspaceWrite),
        ModelId::new(""),
    );

    provider
        .complete(simple_request())
        .await
        .expect("completion");

    let argv = fs::read_to_string(dir.path().join("argv.txt")).expect("argv file");
    assert!(
        argv.lines()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|w| w == ["-s", "workspace-write"]),
        "expected -s workspace-write after the explicit opt-in:\n{argv}"
    );
}

#[tokio::test]
async fn mocked_subprocess_failure_returns_upstream_error() {
    // Shim exits non-zero with a generic stderr (not a login failure). It does
    // NOT drain stdin, so writing the prompt races the child's exit — exercising
    // the BrokenPipe-tolerance path: the exit status, not the pipe error, wins.
    let (_dir, shim) = write_shim(
        "#!/bin/sh\n\
         echo 'boom: simulated codex internal error' >&2\n\
         exit 1\n",
    );
    let provider = CodexProvider::new(
        CodexConfig::new().codex_binary(shim),
        ModelId::new("gpt-5-codex"),
    );

    let err = provider.complete(simple_request()).await.unwrap_err();
    match err {
        ProviderError::Upstream(msg) => {
            assert!(msg.contains("simulated codex internal error"), "got: {msg}");
            assert!(msg.contains("status 1"), "got: {msg}");
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn mocked_login_failure_returns_unauthorized() {
    // Shim exits non-zero with a login-style stderr → Unauthorized.
    let (_dir, shim) = write_shim(
        "#!/bin/sh\n\
         echo 'Not logged in. Please run `codex login`.' >&2\n\
         exit 1\n",
    );
    let provider = CodexProvider::new(
        CodexConfig::new().codex_binary(shim),
        ModelId::new("gpt-5-codex"),
    );

    let err = provider.complete(simple_request()).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::Unauthorized),
        "expected Unauthorized, got {err:?}"
    );
}

#[tokio::test]
async fn mocked_rate_limit_returns_rate_limited() {
    // Shim exits non-zero with a quota-style stderr → RateLimited, not a
    // credential failure even though the diagnostic names a key.
    let (_dir, shim) = write_shim(
        "#!/bin/sh\n\
         echo 'Error code: 429 - rate limit exceeded for this API key' >&2\n\
         exit 1\n",
    );
    let provider = CodexProvider::new(
        CodexConfig::new().codex_binary(shim),
        ModelId::new("gpt-5-codex"),
    );

    let err = provider.complete(simple_request()).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::RateLimited { .. }),
        "expected RateLimited, got {err:?}"
    );
}

#[tokio::test]
async fn an_echoed_secret_is_redacted_from_the_error() {
    // Shim exits non-zero echoing a secret-shaped key on stderr → the
    // diagnostic must reach ProviderError redacted.
    let (_dir, shim) = write_shim(
        "#!/bin/sh\n\
         echo 'request failed for API key sk-abcdefghij0123456789' >&2\n\
         exit 1\n",
    );
    let provider = CodexProvider::new(
        CodexConfig::new().codex_binary(shim),
        ModelId::new("gpt-5-codex"),
    );

    let err = provider.complete(simple_request()).await.unwrap_err();
    match err {
        ProviderError::Upstream(msg) => {
            assert!(
                !msg.contains("sk-abcdefghij0123456789"),
                "secret must not survive redaction: {msg}"
            );
            assert!(
                msg.contains("<REDACTED>"),
                "expected a redaction marker, got: {msg}"
            );
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn turn_failed_event_fails_closed_even_on_zero_exit() {
    // A child that exits 0 but emitted turn.failed must not look like a
    // clean success.
    let (_dir, shim) = write_shim(
        "#!/bin/sh\n\
         cat > /dev/null\n\
         printf '%s\\n' '{\"type\":\"turn.failed\",\"error\":{\"message\":\"model overloaded\"}}'\n",
    );
    let provider = CodexProvider::new(
        CodexConfig::new().codex_binary(shim),
        ModelId::new("gpt-5-codex"),
    );

    let err = provider.complete(simple_request()).await.unwrap_err();
    match err {
        ProviderError::Upstream(msg) => assert!(
            msg.contains("model overloaded"),
            "expected the event diagnostic, got: {msg}"
        ),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn plain_text_output_falls_back_to_stripped_stdout() {
    // Shim emits no JSON events, just ANSI-colored plain text → the fallback
    // path strips ANSI and uses the raw stdout as content.
    let (_dir, shim) = write_shim(
        "#!/bin/sh\n\
         cat > /dev/null\n\
         printf '\\033[1;32mplain answer\\033[0m\\n'\n",
    );
    let provider = CodexProvider::new(
        CodexConfig::new().codex_binary(shim),
        ModelId::new("gpt-5-codex"),
    );

    let resp = provider
        .complete(simple_request())
        .await
        .expect("completion");
    assert_eq!(resp.content, "plain answer");
    // No usage events → zeroed usage, zero cost.
    assert_eq!(resp.usage.tokens_in, 0);
    assert_eq!(resp.cost.cents, 0);
}

#[tokio::test]
async fn non_json_stdout_noise_does_not_abort_a_valid_turn() {
    let (_dir, shim) = write_shim(
        "#!/bin/sh\n\
         cat > /dev/null\n\
         echo 'INFO codex-core: workspace scanned'\n\
         printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"SURVIVED-THE-NOISE\"}}'\n",
    );
    let provider = CodexProvider::new(
        CodexConfig::new().codex_binary(shim),
        ModelId::new("gpt-5-codex"),
    );

    let resp = provider
        .complete(simple_request())
        .await
        .expect("noise must not abort an otherwise valid turn");
    assert_eq!(resp.content, "SURVIVED-THE-NOISE");
}

#[tokio::test]
async fn empty_output_is_a_failure_not_an_empty_success() {
    // Shim drains stdin and prints nothing at all, exiting 0.
    let (_dir, shim) = write_shim("#!/bin/sh\ncat > /dev/null\n");
    let provider = CodexProvider::new(
        CodexConfig::new().codex_binary(shim),
        ModelId::new("gpt-5-codex"),
    );

    let err = provider.complete(simple_request()).await.unwrap_err();
    match err {
        ProviderError::Upstream(msg) => assert!(
            msg.contains("no parseable output"),
            "expected an empty-output diagnostic, got: {msg}"
        ),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn timeout_returns_network_failure() {
    use std::time::Duration;
    // Shim sleeps past the configured timeout; kill_on_drop reaps it.
    let (_dir, shim) = write_shim("#!/bin/sh\nsleep 30\n");
    let provider = CodexProvider::new(
        CodexConfig::new()
            .codex_binary(shim)
            .request_timeout(Duration::from_millis(300)),
        ModelId::new("gpt-5-codex"),
    );

    let err = provider.complete(simple_request()).await.unwrap_err();
    match err {
        ProviderError::NetworkFailure(msg) => {
            assert!(msg.contains("exceeded"), "got: {msg}")
        }
        other => panic!("expected NetworkFailure timeout, got {other:?}"),
    }
}

#[tokio::test]
async fn empty_prompt_is_rejected_before_spawning_a_child() {
    let provider = CodexProvider::new(
        CodexConfig::new().codex_binary("/nonexistent/must-not-spawn"),
        ModelId::new("gpt-5-codex"),
    );
    let mut req = simple_request();
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
async fn an_unenforceable_output_ceiling_is_refused_not_ignored() {
    // codex exec has no per-request output cap, so a ceiling below the floor
    // must fail rather than be silently discarded.
    let provider = CodexProvider::new(
        CodexConfig::new().codex_binary("/nonexistent/must-not-spawn"),
        ModelId::new("gpt-5-codex"),
    );
    let mut req = simple_request();
    req.max_tokens = 64;

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
    let (dir, shim) = write_shim(
        "#!/bin/sh\n\
         cat > /dev/null\n\
         printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"FLOOR-OK\"}}'\n",
    );
    let provider = CodexProvider::new(
        CodexConfig::new().codex_binary(shim).max_tokens_floor(0),
        ModelId::new("gpt-5-codex"),
    );

    let mut req = simple_request();
    req.max_tokens = 64;
    let resp = provider
        .complete(req)
        .await
        .expect("zero floor must accept any ceiling");
    assert_eq!(resp.content, "FLOOR-OK");
    drop(dir);
}

#[tokio::test]
async fn a_zero_max_tokens_delegates_regardless_of_floor() {
    let (_dir, shim) = write_shim(
        "#!/bin/sh\n\
         cat > /dev/null\n\
         printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ZERO-CEILING-OK\"}}'\n",
    );
    let provider = CodexProvider::new(
        CodexConfig::new().codex_binary(shim),
        ModelId::new("gpt-5-codex"),
    );

    let mut req = simple_request();
    req.max_tokens = 0;
    let resp = provider
        .complete(req)
        .await
        .expect("max_tokens=0 must delegate");
    assert_eq!(resp.content, "ZERO-CEILING-OK");
}

/// Regression for the §3.3b `ETXTBSY` ("Text file busy") spawn flake on Linux.
///
/// In a multithreaded program that both writes executables and spawns
/// subprocesses, a sibling thread's `fork()`+`execve()` transiently inherits a
/// just-written shim's writable fd across the fork window, so our `execve` of
/// that shim races to `ETXTBSY` even though our own writer was already closed.
/// `spawn_codex` retries that errno away. This hammers write-then-spawn
/// concurrently across several worker threads — the shape that provokes the
/// race — and asserts it never surfaces as a spawn failure. On macOS `ETXTBSY`
/// is unreachable, so the loop simply exercises the happy path there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_after_write_no_etxtbsy() {
    // Concurrency across worker threads is what provokes the inherited-fd race;
    // 4 writers × 25 spawns each keeps the run short while still overlapping many
    // fork→exec windows against freshly-written shims.
    const TASKS: usize = 4;
    const ITERS: usize = 25;
    let shim_body = "#!/bin/sh\n\
         cat > /dev/null\n\
         printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}'\n";

    let mut handles = Vec::with_capacity(TASKS);
    for _ in 0..TASKS {
        handles.push(tokio::spawn(async move {
            for _ in 0..ITERS {
                // Fresh tempdir + shim per iteration: no path collision, so the
                // only way a spawn can see a busy file is the inherited-fd race.
                let (_dir, shim) = write_shim(shim_body);
                let provider = CodexProvider::new(
                    CodexConfig::new().codex_binary(shim),
                    ModelId::new("gpt-5-codex"),
                );
                match provider.complete(simple_request()).await {
                    Ok(resp) => assert_eq!(resp.content, "ok"),
                    Err(ProviderError::Upstream(msg)) => {
                        assert!(
                            !msg.to_ascii_lowercase().contains("text file busy"),
                            "ETXTBSY leaked through the spawn retry: {msg}"
                        );
                        panic!("unexpected spawn failure: {msg}");
                    }
                    Err(other) => panic!("unexpected error: {other:?}"),
                }
            }
        }));
    }
    for h in handles {
        h.await.expect("spawn-stress task panicked");
    }
}
