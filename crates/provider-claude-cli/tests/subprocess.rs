//! Subprocess round-trip tests for the §3.3c Claude CLI provider.
//!
//! These drive [`ClaudeCliProvider::complete`] against a tiny executable shim
//! that stands in for the real `claude` binary — emitting known JSON on success,
//! or failing on purpose — so the spawn + stdin + parse plumbing is exercised
//! with no claude install and no subscription spend. The shim is a POSIX `sh`
//! script, so the suite is `#[cfg(unix)]` (CI runs on macOS/Linux).
//!
//! A gated live test (`claude_cli_live_smoke`) hits the real CLI only when
//! `CLAUDE_CLI_LIVE_TEST=1` is set.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use ardur_provider_claude_cli::{ClaudeCliConfig, ClaudeCliProvider, PermissionMode};
use ardur_provider_runtime::{CompletionRequest, ModelId, Provider, ProviderError};
use ardur_runtime::ChatMessage;
use tempfile::TempDir;

/// Write `body` as an executable shim into a fresh tempdir, returning the dir
/// (kept alive for the test) and the shim path.
fn write_shim(body: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("claude-shim.sh");
    fs::write(&path, body).expect("write shim");
    let mut perms = fs::metadata(&path).expect("stat shim").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod shim");
    (dir, path)
}

fn simple_request() -> CompletionRequest {
    CompletionRequest::new(vec![ChatMessage::user("ping")], ModelId::new("sonnet"), 64)
}

/// A shim emitting the real CLI's event-array shape ending in a `result` object.
fn success_shim_body() -> &'static str {
    "#!/bin/sh\n\
     cat > /dev/null\n\
     printf '%s\\n' '[{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\"},{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"shim-pong\"}]}},{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"shim-pong\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":12,\"output_tokens\":3}}]'\n"
}

#[tokio::test]
async fn binary_not_found_returns_upstream_error() {
    // A binary that does not exist → the "not installed" Upstream error (the
    // ProviderError taxonomy has no dedicated ConfigError variant; see lib docs).
    let provider = ClaudeCliProvider::new(
        ClaudeCliConfig::new().claude_binary("/nonexistent/path/to/claude-xyz"),
        ModelId::new("sonnet"),
    );
    let err = provider.complete(simple_request()).await.unwrap_err();
    match err {
        ProviderError::Upstream(msg) => {
            assert!(
                msg.contains("Claude CLI not installed"),
                "expected not-installed message, got: {msg}"
            );
        }
        other => panic!("expected Upstream not-installed, got {other:?}"),
    }
}

#[tokio::test]
async fn mocked_subprocess_returns_response() {
    let (_dir, shim) = write_shim(success_shim_body());
    let provider = ClaudeCliProvider::new(
        ClaudeCliConfig::new()
            .claude_binary(shim)
            .permission_mode(PermissionMode::Default),
        ModelId::new("sonnet"),
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
    assert!(resp.raw_provider_response.is_some());
}

#[tokio::test]
async fn mocked_subprocess_failure_returns_upstream_error() {
    // Shim exits non-zero with a generic stderr (not a login/rate failure). It
    // does NOT drain stdin, so writing the prompt races the child's exit —
    // exercising the BrokenPipe-tolerance path: the exit status, not the pipe
    // error, wins.
    let (_dir, shim) = write_shim(
        "#!/bin/sh\n\
         echo 'boom: simulated claude internal error' >&2\n\
         exit 1\n",
    );
    let provider = ClaudeCliProvider::new(
        ClaudeCliConfig::new().claude_binary(shim),
        ModelId::new("sonnet"),
    );

    let err = provider.complete(simple_request()).await.unwrap_err();
    match err {
        ProviderError::Upstream(msg) => {
            assert!(
                msg.contains("simulated claude internal error"),
                "got: {msg}"
            );
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
         echo 'Invalid API key · Please run `claude login`.' >&2\n\
         exit 1\n",
    );
    let provider = ClaudeCliProvider::new(
        ClaudeCliConfig::new().claude_binary(shim),
        ModelId::new("sonnet"),
    );

    let err = provider.complete(simple_request()).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::Unauthorized),
        "expected Unauthorized, got {err:?}"
    );
}

#[tokio::test]
async fn mocked_rate_limit_returns_rate_limited() {
    // Shim exits non-zero with a credit-pool/usage-limit stderr → RateLimited.
    let (_dir, shim) = write_shim(
        "#!/bin/sh\n\
         echo 'Usage limit reached: your Agent SDK credit balance is too low.' >&2\n\
         exit 1\n",
    );
    let provider = ClaudeCliProvider::new(
        ClaudeCliConfig::new().claude_binary(shim),
        ModelId::new("sonnet"),
    );

    let err = provider.complete(simple_request()).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::RateLimited { .. }),
        "expected RateLimited, got {err:?}"
    );
}

#[tokio::test]
async fn mocked_in_band_error_result_is_classified() {
    // Exit code 0 but the result object reports is_error:true → classified from
    // the result text (here, a login error → Unauthorized).
    let (_dir, shim) = write_shim(
        "#!/bin/sh\n\
         cat > /dev/null\n\
         printf '%s\\n' '[{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"Not logged in. Run claude login.\",\"stop_reason\":null}]'\n",
    );
    let provider = ClaudeCliProvider::new(
        ClaudeCliConfig::new().claude_binary(shim),
        ModelId::new("sonnet"),
    );

    let err = provider.complete(simple_request()).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::Unauthorized),
        "expected Unauthorized from in-band error, got {err:?}"
    );
}

#[tokio::test]
async fn unparseable_success_output_returns_upstream() {
    // Exit 0 but stdout is not JSON → Upstream (no InvalidResponse variant).
    let (_dir, shim) = write_shim(
        "#!/bin/sh\n\
         cat > /dev/null\n\
         printf '%s\\n' 'this is not json'\n",
    );
    let provider = ClaudeCliProvider::new(
        ClaudeCliConfig::new().claude_binary(shim),
        ModelId::new("sonnet"),
    );

    let err = provider.complete(simple_request()).await.unwrap_err();
    match err {
        ProviderError::Upstream(msg) => assert!(msg.contains("not valid JSON"), "got: {msg}"),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn timeout_returns_network_failure() {
    use std::time::Duration;
    // Shim sleeps past the configured timeout; kill_on_drop reaps it.
    let (_dir, shim) = write_shim("#!/bin/sh\nsleep 30\n");
    let provider = ClaudeCliProvider::new(
        ClaudeCliConfig::new()
            .claude_binary(shim)
            .request_timeout(Duration::from_millis(300)),
        ModelId::new("sonnet"),
    );

    let err = provider.complete(simple_request()).await.unwrap_err();
    match err {
        ProviderError::NetworkFailure(msg) => assert!(msg.contains("timed out"), "got: {msg}"),
        other => panic!("expected NetworkFailure timeout, got {other:?}"),
    }
}

/// Regression for the §3.3d `ETXTBSY` ("Text file busy") spawn flake on Linux.
///
/// In a multithreaded program that both writes executables and spawns
/// subprocesses, a sibling thread's `fork()`+`execve()` transiently inherits a
/// just-written shim's writable fd across the fork window, so our `execve` of
/// that shim races to `ETXTBSY` even though our own writer was already closed.
/// `spawn_claude` retries that errno away (the same race + fix as the §3.3b
/// codex backend, PR #82). This hammers write-then-spawn concurrently across
/// several worker threads — the shape that provokes the race — and asserts it
/// never surfaces as a spawn failure. On macOS `ETXTBSY` is unreachable, so the
/// loop simply exercises the happy path there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_after_write_no_etxtbsy() {
    // Concurrency across worker threads is what provokes the inherited-fd race;
    // 4 writers × 25 spawns each keeps the run short while still overlapping many
    // fork→exec windows against freshly-written shims.
    const TASKS: usize = 4;
    const ITERS: usize = 25;

    let mut handles = Vec::with_capacity(TASKS);
    for _ in 0..TASKS {
        handles.push(tokio::spawn(async move {
            for _ in 0..ITERS {
                // Fresh tempdir + shim per iteration: no path collision, so the
                // only way a spawn can see a busy file is the inherited-fd race.
                let (_dir, shim) = write_shim(success_shim_body());
                let provider = ClaudeCliProvider::new(
                    ClaudeCliConfig::new().claude_binary(shim),
                    ModelId::new("sonnet"),
                );
                match provider.complete(simple_request()).await {
                    Ok(resp) => assert_eq!(resp.content, "shim-pong"),
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

/// Gated live smoke test against the real `claude` CLI. Off by default; runs only
/// when `CLAUDE_CLI_LIVE_TEST=1` and a logged-in claude install is present.
#[tokio::test]
async fn claude_cli_live_smoke() {
    if std::env::var("CLAUDE_CLI_LIVE_TEST").as_deref() != Ok("1") {
        eprintln!("skipping claude_cli_live_smoke (set CLAUDE_CLI_LIVE_TEST=1 to run)");
        return;
    }
    let provider = ClaudeCliProvider::from_env(ModelId::new(""));
    let req = CompletionRequest::new(
        vec![ChatMessage::user("Reply with exactly the word: pong")],
        ModelId::new(""),
        64,
    );
    let resp = provider
        .complete(req)
        .await
        .expect("live claude completion");
    assert!(
        !resp.content.is_empty(),
        "live claude returned empty content"
    );
    eprintln!("live claude replied: {:?}", resp.content);
}
