//! ARD-457 — the hardened §6.1 built-in tools default-register, and become
//! *invokable*, only under the operator's opt-ins.
//!
//! Two properties are asserted end-to-end over a real [`AppState::boot`]:
//! 1. **Fail-closed default.** With no opt-ins, `shell.run`/`http.fetch`/`file.*`
//!    are absent from the registry the runtime invokes, and their `cap.*`
//!    capabilities are absent from the runtime cap-token allowlist — so a prompt
//!    that names them is denied (`CapDenied`), exactly as before ARD-457.
//! 2. **Granted ⇒ registered *and* cap-aligned.** When the opts are set, the
//!    tools are registered *and* their capabilities appear in
//!    [`AppState::tool_allowlist`] — the allowlist the per-turn cap-token is
//!    minted from — so the runtime will actually let the model invoke them.
//!
//! Property (2) is the crux of the ARD-457 correctness subtlety: registration
//! alone is insufficient (a registered-but-uncapped tool is dead on
//! `CapDenied`), and the server derives the cap-token allowlist from the
//! registered tool set, so registering the tool is what closes both halves.

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use ardur_cap_token::KeyPair;
use ardur_provider_runtime::{AnthropicProvider, ModelId, Provider};
use ardur_server::{AppState, Config, assemble_tool_registry};
use ardur_tool_registry::{
    BuiltinOpts, HttpFetchOpts, HttpFetchTool, ListDirTool, ReadFileTool, ShellTool, ToolId,
    ToolRegistry, WriteFileTool,
};

/// Assemble the runtime tool registry with `opts` applied (no skills, no remote
/// MCP servers) — the same path `ardur-server` uses at boot.
async fn assemble(opts: BuiltinOpts) -> ToolRegistry {
    // A throwaway cap-root key: the delegate_task tool registers against it but
    // these tests never mint/verify tokens, so any valid Ed25519 public key works.
    let cap_root = KeyPair::new().public();
    assemble_tool_registry("stub", "in-memory", &[] as &[PathBuf], &[], cap_root, opts).await
}

/// Boot an `AppState` over the stub provider with the already-assembled `tools`.
async fn boot(config: &Config, tools: Arc<ToolRegistry>) -> Arc<AppState> {
    let provider: Arc<dyn Provider> =
        Arc::new(AnthropicProvider::stub(ModelId::new(&config.model)));
    AppState::boot(config, provider, tools)
        .await
        .expect("AppState boots")
}

#[tokio::test]
async fn default_boot_registers_no_hardened_tool_and_grants_no_hardened_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);

    let tools = assemble(BuiltinOpts::default()).await;
    for id in [
        ShellTool::ID,
        HttpFetchTool::ID,
        ReadFileTool::ID,
        WriteFileTool::ID,
        ListDirTool::ID,
    ] {
        assert!(
            tools.get(&ToolId::new(id)).is_none(),
            "`{id}` must not register without an opt-in"
        );
    }

    let state = boot(&config, Arc::new(tools)).await;
    let allow = state.tool_allowlist();
    for cap in [
        "cap.shell_exec",
        "cap.process_spawn",
        "cap.network_out",
        "cap.fs_read",
        "cap.fs_write",
    ] {
        assert!(
            !allow.contains(&cap.to_string()),
            "`{cap}` must be absent from the runtime cap-token allowlist by default (got {allow:?})"
        );
    }
}

#[tokio::test]
async fn shell_and_http_opt_in_registers_and_cap_aligns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);

    let opts = BuiltinOpts {
        enable_shell: true,
        shell_allowlist: Some(vec!["echo".to_string()]),
        file_root: None,
        http: Some(HttpFetchOpts {
            enable: true,
            allowlist: vec!["example.com".to_string()],
            ..HttpFetchOpts::default()
        }),
        enable_media: false,
    };
    let tools = assemble(opts).await;
    assert!(
        tools.get(&ToolId::new(ShellTool::ID)).is_some(),
        "shell.run registers when enabled"
    );
    assert!(
        tools.get(&ToolId::new(HttpFetchTool::ID)).is_some(),
        "http.fetch registers when enabled"
    );
    // File tools were not granted here, so they stay absent.
    assert!(tools.get(&ToolId::new(ReadFileTool::ID)).is_none());

    let state = boot(&config, Arc::new(tools)).await;
    let allow = state.tool_allowlist();

    // The tool ids themselves are on the allowlist...
    assert!(allow.contains(&ShellTool::ID.to_string()));
    assert!(allow.contains(&HttpFetchTool::ID.to_string()));
    // ...and — the ARD-457 point — so are the `cap.*` labels the runtime enforces
    // at invoke time, so these tools are actually usable rather than dead on
    // `CapDenied`.
    for cap in ["cap.shell_exec", "cap.process_spawn", "cap.network_out"] {
        assert!(
            allow.contains(&cap.to_string()),
            "granted tool's `{cap}` must be minted into the runtime cap-token (got {allow:?})"
        );
    }
    // File capabilities were not granted, so they remain denied.
    assert!(!allow.contains(&"cap.fs_read".to_string()));
    assert!(!allow.contains(&"cap.fs_write".to_string()));
}

#[tokio::test]
async fn file_tool_root_opt_in_registers_file_tools_and_grants_fs_caps() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);
    let file_root = tempfile::tempdir().expect("file root tempdir");

    let opts = BuiltinOpts {
        file_root: Some(file_root.path().to_path_buf()),
        ..BuiltinOpts::default()
    };
    let tools = assemble(opts).await;
    for id in [ReadFileTool::ID, WriteFileTool::ID, ListDirTool::ID] {
        assert!(
            tools.get(&ToolId::new(id)).is_some(),
            "`{id}` registers when a file root is configured"
        );
    }
    // Shell/http were not granted.
    assert!(tools.get(&ToolId::new(ShellTool::ID)).is_none());

    let state = boot(&config, Arc::new(tools)).await;
    let allow = state.tool_allowlist();
    assert!(allow.contains(&"cap.fs_read".to_string()));
    assert!(allow.contains(&"cap.fs_write".to_string()));
    assert!(
        !allow.contains(&"cap.shell_exec".to_string()),
        "shell capability must not leak in from the file-tool grant"
    );
}
