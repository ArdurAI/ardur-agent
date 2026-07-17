//! Scenario §3.3b — `codex_provider`.
//!
//! Drives one happy-path turn through the *fused* substrate with the subprocess
//! [`CodexProvider`] as the model backend, pointed at an executable shim that
//! stands in for the real `codex` CLI. It proves the §3.3b subscription provider
//! plugs into the same `FusedRuntime` spine the HTTP backends use (cap-token →
//! cedar → cost-gate → provider → receipt → finalize), that the token usage
//! codex reports is attributed onto the turn's receipt, and that the call is
//! priced at **zero cents** (subscription-billed, not metered).
//!
//! CI runs this offline: the "provider" is a local `sh` shim emitting known
//! JSONL, so there is no codex install and no ChatGPT subscription spend. The
//! shim makes the suite Unix-only.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use ardur_e2e_tests::fixtures;

use ardur_provider_codex::{CodexConfig, CodexProvider, SandboxMode};
use ardur_provider_runtime::{ModelId, Provider};
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};
use tempfile::TempDir;

/// Write an executable `codex` shim emitting a fixed JSONL event stream.
fn codex_shim() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("codex-shim.sh");
    fs::write(
        &path,
        "#!/bin/sh\n\
         cat > /dev/null\n\
         printf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"t-e2e\"}'\n\
         printf '%s\\n' '{\"type\":\"turn.started\"}'\n\
         printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"agent_message\",\"text\":\"codex-pong\"}}'\n\
         printf '%s\\n' '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":33,\"output_tokens\":5}}'\n",
    )
    .expect("write shim");
    let mut perms = fs::metadata(&path).expect("stat").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod");
    (dir, path)
}

#[tokio::test]
async fn turn_through_fused_substrate_with_codex_backend() {
    // ---- The shim standing in for the real `codex` CLI.
    let (_shim_dir, shim) = codex_shim();

    // ---- The §3.3b provider, invoking the shim instead of the installed codex.
    let provider: Arc<dyn Provider> = Arc::new(CodexProvider::new(
        CodexConfig::new()
            .codex_binary(shim)
            .sandbox_mode(SandboxMode::ReadOnly),
        ModelId::new("gpt-5-codex"),
    ));
    assert_eq!(provider.id().0, "codex");

    // ---- The fused runtime, wired with the Codex backend.
    let runtime = fixtures::fused_builder(provider)
        .build()
        .expect("the fused runtime wires with the Codex provider");

    let token = fixtures::dev_valid_cap_token();
    let result = runtime
        .submit(SubmitRequest {
            messages: vec![ChatMessage::user("ping through codex")],
            cap_token: CapTokenRef(token),
            session_id: SessionId::new(),
            requested_provider: None,
        })
        .await
        .expect("the turn completes through the Codex-backed substrate");

    // The assistant text is the shim's agent_message.
    assert_eq!(result.response.content, "codex-pong");
    // The usage codex reported is attributed onto the turn's receipt cost.
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
