//! Scenario §3.3c — `claude_cli_provider`.
//!
//! Drives one happy-path turn through the *fused* substrate with the subprocess
//! [`ClaudeCliProvider`] as the model backend, pointed at an executable shim
//! that stands in for the real `claude` CLI. It proves the §3.3c subscription
//! provider plugs into the same `FusedRuntime` spine the HTTP backends use
//! (cap-token → cedar → cost-gate → provider → receipt → finalize), that the
//! token usage the CLI reports is attributed onto the turn's receipt, and that
//! the call is priced at **zero cents** (subscription-billed, not metered).
//!
//! CI runs this offline: the "provider" is a local `sh` shim emitting the CLI's
//! known JSON event-array shape, so there is no claude install and no
//! subscription spend. The shim makes the suite Unix-only.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use ardur_e2e_tests::fixtures;

use ardur_provider_claude_cli::{ClaudeCliConfig, ClaudeCliProvider, PermissionMode};
use ardur_provider_runtime::{ModelId, Provider};
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};
use tempfile::TempDir;

/// Write an executable `claude` shim emitting a fixed JSON event array ending in
/// a `result` object (the real `claude -p --output-format json` shape).
fn claude_shim() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("claude-shim.sh");
    fs::write(
        &path,
        "#!/bin/sh\n\
         cat > /dev/null\n\
         printf '%s\\n' '[{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"t-e2e\"},{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"claude-pong\"}]}},{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"claude-pong\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":33,\"output_tokens\":5}}]'\n",
    )
    .expect("write shim");
    let mut perms = fs::metadata(&path).expect("stat").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod");
    (dir, path)
}

#[tokio::test]
async fn turn_through_fused_substrate_with_claude_cli_backend() {
    // ---- The shim standing in for the real `claude` CLI.
    let (_shim_dir, shim) = claude_shim();

    // ---- The §3.3c provider, invoking the shim instead of the installed claude.
    let provider: Arc<dyn Provider> = Arc::new(ClaudeCliProvider::new(
        ClaudeCliConfig::new()
            .claude_binary(shim)
            .permission_mode(PermissionMode::Default),
        ModelId::new("sonnet"),
    ));
    assert_eq!(provider.id().0, "claude-cli");

    // ---- The fused runtime, wired with the Claude CLI backend.
    let runtime = fixtures::fused_builder(provider)
        .build()
        .expect("the fused runtime wires with the Claude CLI provider");

    let token = fixtures::dev_valid_cap_token();
    let result = runtime
        .submit(SubmitRequest {
            messages: vec![ChatMessage::user("ping through claude")],
            cap_token: CapTokenRef(token),
            session_id: SessionId::new(),
            requested_provider: None,
        })
        .await
        .expect("the turn completes through the Claude-CLI-backed substrate");

    // The assistant text is the shim's result content.
    assert_eq!(result.response.content, "claude-pong");
    // The usage the CLI reported is attributed onto the turn's receipt cost.
    assert_eq!(result.cost.tokens_in, 33);
    assert_eq!(result.cost.tokens_out, 5);
    // Subscription-billed: tokens flow through, but the call costs zero cents.
    assert_eq!(result.cost.cents, 0);
    // A receipt was minted for the turn.
    assert!(
        !result.receipt_id.0.is_nil(),
        "a non-nil receipt id was minted"
    );
}
