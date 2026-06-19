//! §6.1 — integration coverage for the built-in shell and file tools: command
//! execution and capture, the shell allowlist, file root containment, write
//! semantics, directory listing, and the `register_builtins` installer.

use std::collections::HashMap;
use std::path::PathBuf;

use ardur_tool_registry::{
    BuiltinOpts, CapTokenRef, InvocationId, ListDirTool, ReadFileTool, SessionId, ShellTool, Tool,
    ToolContext, ToolError, ToolId, ToolRegistry, WriteFileTool,
};
use serde_json::json;
use tempfile::TempDir;

/// A throwaway context rooted at `cwd` with a wide budget.
fn ctx(cwd: PathBuf) -> ToolContext {
    ToolContext {
        cap_token: CapTokenRef(String::new()),
        session_id: SessionId::new(),
        invocation_id: InvocationId::new(),
        cwd,
        env: HashMap::new(),
        cost_budget_cents: u32::MAX,
    }
}

// ── shell.run ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn shell_runs_simple_command() {
    let tool = ShellTool::without_allowlist();
    let out = tool
        .invoke(&ctx(PathBuf::from(".")), json!({ "command": "echo hello" }))
        .await
        .expect("echo runs");

    assert_eq!(out.content["stdout"], "hello\n");
    assert_eq!(out.content["exit_code"], 0);
    assert_eq!(out.content["timed_out"], false);
}

#[tokio::test]
async fn shell_captures_stdout_stderr_exit_code() {
    let tool = ShellTool::without_allowlist();
    let out = tool
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "command": "echo out; echo err 1>&2; exit 3" }),
        )
        .await
        .expect("command runs");

    assert_eq!(out.content["stdout"], "out\n");
    assert_eq!(out.content["stderr"], "err\n");
    assert_eq!(out.content["exit_code"], 3);
    assert_eq!(out.content["timed_out"], false);
}

#[tokio::test]
async fn shell_timeout_aborts() {
    let tool = ShellTool::without_allowlist();
    let out = tool
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "command": "sleep 10", "timeout_secs": 1 }),
        )
        .await
        .expect("invocation returns a timed-out result, not an error");

    assert_eq!(out.content["timed_out"], true);
    assert_eq!(out.content["exit_code"], -1);
}

#[tokio::test]
async fn shell_allowlist_blocks_disallowed() {
    let tool = ShellTool::with_allowlist(vec!["echo".to_string()]);
    let err = tool
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "command": "rm -rf /tmp/x" }),
        )
        .await
        .expect_err("disallowed command is denied");

    assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
}

#[tokio::test]
async fn shell_allowlist_permits_allowed() {
    let tool = ShellTool::with_allowlist(vec!["ls|echo|cat".to_string()]);
    let out = tool
        .invoke(&ctx(PathBuf::from(".")), json!({ "command": "echo ok" }))
        .await
        .expect("allowlisted command runs");

    assert_eq!(out.content["stdout"], "ok\n");
    assert_eq!(out.content["exit_code"], 0);
}

// ── file.read / file.write / file.list ───────────────────────────────────────

#[tokio::test]
async fn file_read_root_relative() {
    let root = TempDir::new().expect("tempdir");
    std::fs::write(root.path().join("note.txt"), "hello root").expect("seed file");

    let tool = ReadFileTool::with_root(root.path().to_path_buf());
    let out = tool
        .invoke(&ctx(PathBuf::from(".")), json!({ "path": "note.txt" }))
        .await
        .expect("read succeeds");

    assert_eq!(out.content["content"], "hello root");
    assert_eq!(out.content["bytes_read"], 10);
    assert_eq!(out.content["truncated"], false);
}

#[tokio::test]
async fn file_read_truncates_at_max_bytes() {
    let root = TempDir::new().expect("tempdir");
    std::fs::write(root.path().join("big.txt"), "0123456789").expect("seed file");

    let tool = ReadFileTool::with_root(root.path().to_path_buf());
    let out = tool
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "path": "big.txt", "max_bytes": 4 }),
        )
        .await
        .expect("read succeeds");

    assert_eq!(out.content["content"], "0123");
    assert_eq!(out.content["bytes_read"], 4);
    assert_eq!(out.content["truncated"], true);
}

#[tokio::test]
async fn file_read_rejects_escape() {
    let root = TempDir::new().expect("tempdir");
    let tool = ReadFileTool::with_root(root.path().to_path_buf());

    let err = tool
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "path": "../../etc/passwd" }),
        )
        .await
        .expect_err("escaping path is denied");

    assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
}

#[tokio::test]
async fn file_write_creates_parent_dirs() {
    let root = TempDir::new().expect("tempdir");
    let tool = WriteFileTool::with_root(root.path().to_path_buf());

    let out = tool
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "path": "nested/deep/out.txt", "content": "body" }),
        )
        .await
        .expect("write succeeds");

    assert_eq!(out.content["bytes_written"], 4);
    let written = std::fs::read_to_string(root.path().join("nested/deep/out.txt"))
        .expect("file exists after write");
    assert_eq!(written, "body");
}

#[tokio::test]
async fn file_write_append_mode() {
    let root = TempDir::new().expect("tempdir");
    let tool = WriteFileTool::with_root(root.path().to_path_buf());
    let c = ctx(PathBuf::from("."));

    tool.invoke(&c, json!({ "path": "log.txt", "content": "a" }))
        .await
        .expect("initial write");
    tool.invoke(
        &c,
        json!({ "path": "log.txt", "content": "b", "mode": "append" }),
    )
    .await
    .expect("append write");

    let written = std::fs::read_to_string(root.path().join("log.txt")).expect("file exists");
    assert_eq!(written, "ab");
}

#[tokio::test]
async fn file_list_returns_entries() {
    let root = TempDir::new().expect("tempdir");
    std::fs::write(root.path().join("a.txt"), "xy").expect("seed file");
    std::fs::create_dir(root.path().join("sub")).expect("seed dir");

    let tool = ListDirTool::with_root(root.path().to_path_buf());
    let out = tool
        .invoke(&ctx(PathBuf::from(".")), json!({ "path": "." }))
        .await
        .expect("list succeeds");

    let entries = out.content["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 2);
    assert_eq!(out.content["truncated"], false);

    let file = entries
        .iter()
        .find(|e| e["name"] == "a.txt")
        .expect("a.txt listed");
    assert_eq!(file["is_dir"], false);
    assert_eq!(file["size_bytes"], 2);

    let dir = entries
        .iter()
        .find(|e| e["name"] == "sub")
        .expect("sub listed");
    assert_eq!(dir["is_dir"], true);
}

#[tokio::test]
async fn file_list_root_containment() {
    let root = TempDir::new().expect("tempdir");
    let tool = ListDirTool::with_root(root.path().to_path_buf());

    let err = tool
        .invoke(&ctx(PathBuf::from(".")), json!({ "path": "../.." }))
        .await
        .expect_err("escaping listing is denied");

    assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
}

// ── register_builtins ────────────────────────────────────────────────────────

#[tokio::test]
async fn register_builtins_skips_disabled_tools() {
    let root = TempDir::new().expect("tempdir");

    // Shell disabled, file tools enabled.
    let mut registry = ToolRegistry::new();
    registry
        .register_builtins(BuiltinOpts {
            enable_shell: false,
            shell_allowlist: None,
            file_root: Some(root.path().to_path_buf()),
            http: None,
            enable_media: false,
        })
        .expect("register file tools");

    assert!(registry.get(&ToolId::new(ShellTool::ID)).is_none());
    assert!(registry.get(&ToolId::new(ReadFileTool::ID)).is_some());
    assert!(registry.get(&ToolId::new(WriteFileTool::ID)).is_some());
    assert!(registry.get(&ToolId::new(ListDirTool::ID)).is_some());

    // Everything disabled: nothing registers.
    let mut empty = ToolRegistry::new();
    empty
        .register_builtins(BuiltinOpts::default())
        .expect("no-op registration");
    assert!(empty.list().is_empty());

    // Shell enabled with an allowlist; no file root.
    let mut shell_only = ToolRegistry::new();
    shell_only
        .register_builtins(BuiltinOpts {
            enable_shell: true,
            shell_allowlist: Some(vec!["echo".to_string()]),
            file_root: None,
            http: None,
            enable_media: false,
        })
        .expect("register shell only");
    assert!(shell_only.get(&ToolId::new(ShellTool::ID)).is_some());
    assert!(shell_only.get(&ToolId::new(ReadFileTool::ID)).is_none());
}
