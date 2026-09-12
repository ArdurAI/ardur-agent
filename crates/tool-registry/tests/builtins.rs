//! §6.1 — integration coverage for the built-in shell and file tools: command
//! execution and capture, the shell allowlist, file root containment, write
//! semantics, directory listing, and the `register_builtins` installer.

use std::collections::HashMap;
use std::path::PathBuf;

use ardur_tool_registry::{
    BuiltinOpts, CapTokenRef, Capability, HttpFetchOpts, HttpFetchTool, InvocationId, ListDirTool,
    ReadFileTool, SessionId, ShellExecTool, ShellTool, Tool, ToolContext, ToolError, ToolId,
    ToolRegistry, WriteFileTool,
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

#[cfg(not(windows))]
#[tokio::test]
async fn shell_destructive_pattern_matrix_is_best_effort() {
    let root = TempDir::new().expect("tempdir");
    let tool = ShellTool::without_allowlist();

    let blocked = [
        (
            "rm -fr option order",
            format!("rm -fr {}", root.path().join("missing").display()),
        ),
        (
            "base64 decode piped into shell",
            "printf ZWNobyBzaG91bGQtbm90LXJ1bgo= | base64 -d | sh".to_string(),
        ),
        (
            "dd with spacing around assignment operators",
            "dd if = /dev/zero of = /dev/null count=0".to_string(),
        ),
    ];

    for (case, command) in blocked {
        let err = tool
            .invoke(&ctx(PathBuf::from(".")), json!({ "command": command }))
            .await
            .expect_err("destructive command should be denied before execution");

        assert!(
            matches!(err, ToolError::Denied { .. }),
            "{case} should match the destructive-pattern denylist, got {err:?}"
        );
    }

    // This denylist is intentionally documented as best-effort rather than a
    // shell parser/sandbox: shell escapes can still hide tokens from the regex
    // layer. Keep the command harmless (the target path does not exist) while
    // preserving the bypass shape in the regression matrix.
    let bypass = format!(r"r\m -fr {}", root.path().join("missing").display());
    let out = tool
        .invoke(&ctx(PathBuf::from(".")), json!({ "command": bypass }))
        .await
        .expect("documented best-effort bypass reaches the shell");
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
            enable_shell_exec: false,
            shell_exec_allowlist: None,
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
            enable_shell_exec: false,
            shell_exec_allowlist: None,
            file_root: None,
            http: None,
            enable_media: false,
        })
        .expect("register shell only");
    assert!(shell_only.get(&ToolId::new(ShellTool::ID)).is_some());
    assert!(shell_only.get(&ToolId::new(ReadFileTool::ID)).is_none());
}

#[tokio::test]
async fn register_builtins_installs_http_only_when_enabled() {
    // `http: None` — no HTTP tool.
    let mut none = ToolRegistry::new();
    none.register_builtins(BuiltinOpts::default())
        .expect("no-op");
    assert!(none.get(&ToolId::new(HttpFetchTool::ID)).is_none());

    // `http: Some { enable: false }` — still skipped.
    let mut disabled = ToolRegistry::new();
    disabled
        .register_builtins(BuiltinOpts {
            http: Some(HttpFetchOpts {
                enable: false,
                ..HttpFetchOpts::default()
            }),
            ..BuiltinOpts::default()
        })
        .expect("disabled http is a no-op");
    assert!(disabled.get(&ToolId::new(HttpFetchTool::ID)).is_none());

    // `http: Some { enable: true }` — registered, and it declares NetworkOut so
    // the runtime cap-token derivation grants `cap.network_out`.
    let mut enabled = ToolRegistry::new();
    enabled
        .register_builtins(BuiltinOpts {
            http: Some(HttpFetchOpts {
                enable: true,
                allowlist: vec!["example.com".to_string()],
                ..HttpFetchOpts::default()
            }),
            ..BuiltinOpts::default()
        })
        .expect("register http");
    let http = enabled
        .get(&ToolId::new(HttpFetchTool::ID))
        .expect("http.fetch is registered");
    assert!(
        http.required_capabilities()
            .contains(&Capability::NetworkOut),
        "http.fetch must declare NetworkOut so `cap.network_out` is minted into the cap-token"
    );
}

/// An operator can only ever register the *allowlisted* shell through
/// `register_builtins` (`Some(list)`). Even an empty allowlist is fail-closed —
/// it denies every command rather than behaving like the unrestricted shell — so
/// there is no configuration of `register_builtins` that yields arbitrary
/// execution.
#[tokio::test]
async fn register_builtins_empty_shell_allowlist_is_fail_closed() {
    let mut registry = ToolRegistry::new();
    registry
        .register_builtins(BuiltinOpts {
            enable_shell: true,
            shell_allowlist: Some(Vec::new()),
            enable_shell_exec: false,
            shell_exec_allowlist: None,
            ..BuiltinOpts::default()
        })
        .expect("register empty-allowlist shell");

    let shell = registry
        .get(&ToolId::new(ShellTool::ID))
        .expect("shell.run is registered");
    let denied = shell
        .invoke(&ctx(PathBuf::from(".")), json!({ "command": "echo hello" }))
        .await;
    assert!(
        matches!(denied, Err(ToolError::Denied { .. })),
        "an empty allowlist denies every command (fail-closed), got: {denied:?}"
    );
}

/// The hardened tools declare exactly the capabilities the server derives its
/// runtime cap-token allowlist from (`cap.<snake_case>`), so registering them is
/// sufficient to make them invokable. This pins that contract.
#[tokio::test]
async fn register_builtins_tools_declare_expected_capabilities() {
    let root = TempDir::new().expect("tempdir");
    let mut registry = ToolRegistry::new();
    registry
        .register_builtins(BuiltinOpts {
            enable_shell: true,
            shell_allowlist: Some(vec!["echo".to_string()]),
            enable_shell_exec: false,
            shell_exec_allowlist: None,
            file_root: Some(root.path().to_path_buf()),
            http: Some(HttpFetchOpts {
                enable: true,
                ..HttpFetchOpts::default()
            }),
            enable_media: false,
        })
        .expect("register the full hardened set");

    let caps_of = |id: &str| -> Vec<String> {
        registry
            .get(&ToolId::new(id))
            .expect("tool registered")
            .required_capabilities()
            .iter()
            .map(Capability::as_str)
            .collect()
    };

    // shell.run headlines ShellExec and names the fork/exec it performs.
    let shell = caps_of(ShellTool::ID);
    assert!(shell.contains(&"cap.shell_exec".to_string()));
    assert!(shell.contains(&"cap.process_spawn".to_string()));
    assert_eq!(caps_of(HttpFetchTool::ID), vec!["cap.network_out"]);
    assert_eq!(caps_of(ReadFileTool::ID), vec!["cap.fs_read"]);
    assert_eq!(caps_of(WriteFileTool::ID), vec!["cap.fs_write"]);
    assert_eq!(caps_of(ListDirTool::ID), vec!["cap.fs_read"]);
}

// ── shell.exec (#420 argv-exec confinement) ──────────────────────────────────

/// The exact bypass from #420: with `shell.run` an allowlisted prefix chains
/// straight into arbitrary execution. `shell.exec` must refuse it.
///
/// This test asserts the *contrast*: it demonstrates the old behaviour is real
/// (the prefix gate admits the chained line) and that the new tool denies it,
/// so the guard cannot silently become vacuous if `permits()` is refactored.
#[cfg(not(windows))]
#[tokio::test]
async fn shell_exec_refuses_the_prefix_gate_chaining_bypass() {
    // Baseline: shell.run's prefix gate ADMITS `git ; id` (it starts with an
    // allowlisted prefix) and the system shell then runs `id`.
    let legacy = ShellTool::with_allowlist(vec!["git".to_string()]);
    let out = legacy
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "command": "git --version ; id" }),
        )
        .await
        .expect("shell.run admits the chained command (the #420 bug)");
    assert!(
        out.content["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("uid="),
        "precondition: shell.run really does chain into `id`; got {:?}",
        out.content["stdout"]
    );

    // shell.exec denies the same line: `;` is not in the safe charset.
    let hardened = ShellExecTool::with_allowlist(vec!["git".to_string()]);
    let err = hardened
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "command": "git --version ; id" }),
        )
        .await
        .expect_err("shell.exec must deny a chained command string");
    assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
}

/// Metacharacters supplied through explicit argv are passed as literal bytes,
/// never interpreted. This is the core argv-exec property.
#[cfg(not(windows))]
#[tokio::test]
async fn shell_exec_passes_metacharacters_as_literal_arguments() {
    let tool = ShellExecTool::with_allowlist(vec!["echo".to_string()]);
    let out = tool
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "argv": ["echo", "; id", "$(id)", "`id`", "&& id", "| id"] }),
        )
        .await
        .expect("argv elements are literal, so this is just an echo");

    let stdout = out.content["stdout"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(stdout, "; id $(id) `id` && id | id\n");
    assert!(
        !stdout.contains("uid="),
        "no argument may ever be interpreted as a command: {stdout:?}"
    );
    assert_eq!(out.content["exit_code"], 0);
}

/// The binary allowlist is an exact match, not a prefix match — so an
/// allowlisted name cannot be extended into a different binary.
#[tokio::test]
async fn shell_exec_allowlist_is_exact_not_prefix() {
    let tool = ShellExecTool::with_allowlist(vec!["echo".to_string()]);

    for argv0 in ["echofoo", "echo-x", "ec"] {
        let err = tool
            .invoke(&ctx(PathBuf::from(".")), json!({ "argv": [argv0, "hi"] }))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "{argv0} must not satisfy an `echo` allowlist, got {err:?}"
        );
    }
}

/// A path-qualified spelling of an allowlisted name is a different argv[0] and
/// must not pass an exact-match allowlist.
#[cfg(not(windows))]
#[tokio::test]
async fn shell_exec_allowlist_does_not_accept_path_qualified_aliases() {
    let tool = ShellExecTool::with_allowlist(vec!["echo".to_string()]);
    let err = tool
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "argv": ["/bin/echo", "hi"] }),
        )
        .await
        .expect_err("/bin/echo is not the allowlisted token `echo`");
    assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
}

#[cfg(not(windows))]
#[tokio::test]
async fn shell_exec_runs_an_allowlisted_binary() {
    let tool = ShellExecTool::with_allowlist(vec!["echo|ls".to_string()]);
    let out = tool
        .invoke(&ctx(PathBuf::from(".")), json!({ "argv": ["echo", "ok"] }))
        .await
        .expect("allowlisted binary runs");

    assert_eq!(out.content["stdout"], "ok\n");
    assert_eq!(out.content["exit_code"], 0);
    assert_eq!(out.content["timed_out"], false);
}

/// The safe-charset gate on the string form rejects every shell operator,
/// including the newline separator that a naive operator denylist misses.
#[tokio::test]
async fn shell_exec_command_string_rejects_shell_syntax() {
    let tool = ShellExecTool::without_allowlist();

    for bad in [
        "echo a; id",
        "echo a && id",
        "echo a | id",
        "echo $(id)",
        "echo `id`",
        "echo a > /tmp/x",
        "echo a\nid",
        "echo 'quoted'",
        "echo \"quoted\"",
        "echo a & id",
        "echo *",
        "echo ~/x",
    ] {
        let err = tool
            .invoke(&ctx(PathBuf::from(".")), json!({ "command": bad }))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "command string {bad:?} must be denied, got {err:?}"
        );
    }
}

/// argv[0] selects what executes, so it is charset-checked even in the
/// explicit-argv form (which otherwise permits arbitrary argument bytes).
#[tokio::test]
async fn shell_exec_rejects_unsafe_binary_names() {
    let tool = ShellExecTool::without_allowlist();

    for bad in ["ec;ho", "echo id", "ec|ho", "$(id)", "echo\nid"] {
        let err = tool
            .invoke(&ctx(PathBuf::from(".")), json!({ "argv": [bad] }))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Denied { .. } | ToolError::InvalidArgs(_)),
            "argv[0] {bad:?} must be refused, got {err:?}"
        );
    }
}

#[tokio::test]
async fn shell_exec_requires_exactly_one_input_form() {
    let tool = ShellExecTool::without_allowlist();

    let both = tool
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "argv": ["echo", "a"], "command": "echo a" }),
        )
        .await
        .unwrap_err();
    assert!(matches!(both, ToolError::InvalidArgs(_)), "got {both:?}");

    let neither = tool
        .invoke(&ctx(PathBuf::from(".")), json!({}))
        .await
        .unwrap_err();
    assert!(
        matches!(neither, ToolError::InvalidArgs(_)),
        "got {neither:?}"
    );

    let empty = tool
        .invoke(&ctx(PathBuf::from(".")), json!({ "argv": [] }))
        .await
        .unwrap_err();
    assert!(matches!(empty, ToolError::InvalidArgs(_)), "got {empty:?}");
}

/// The destructive-pattern denylist still applies to the hardened path.
#[cfg(not(windows))]
#[tokio::test]
async fn shell_exec_still_blocks_destructive_patterns() {
    let tool = ShellExecTool::without_allowlist();
    let err = tool
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "argv": ["rm", "-rf", "/tmp/definitely-not-here"] }),
        )
        .await
        .expect_err("destructive pattern is blocked even with argv exec");
    assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
}

/// `shell.exec` must declare exactly the same capabilities as `shell.run`, so
/// hardening the exec path does not quietly widen what a grant authorizes.
#[test]
fn shell_exec_declares_the_same_capabilities_as_shell_run() {
    let exec = ShellExecTool::without_allowlist();
    let run = ShellTool::without_allowlist();

    let mut exec_caps = exec.required_capabilities().to_vec();
    let mut run_caps = run.required_capabilities().to_vec();
    exec_caps.sort_by_key(|c| format!("{c:?}"));
    run_caps.sort_by_key(|c| format!("{c:?}"));

    assert_eq!(exec_caps, run_caps);
    assert!(exec_caps.contains(&Capability::ShellExec));
    assert!(exec_caps.contains(&Capability::ProcessSpawn));
}

/// `sleep` is a Unix binary; the Windows path spawns nothing and would return
/// ExecutionFailed before reaching the timeout assertions.
#[cfg(not(windows))]
#[tokio::test]
async fn shell_exec_honours_timeout() {
    let tool = ShellExecTool::with_allowlist(vec!["sleep".to_string()]);
    let out = tool
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "argv": ["sleep", "5"], "timeout_secs": 1 }),
        )
        .await
        .expect("timeout is reported, not an error");

    assert_eq!(out.content["timed_out"], true);
    assert_eq!(out.content["exit_code"], -1);
}

// ── shell.exec: PR #441 review findings ─────────────────────────────────────

/// P2: the destructive-pattern denylist must inspect the binary and its flags,
/// not a flattened argument string. An allowlisted `echo` asked to PRINT the
/// text "rm -rf /tmp/x" can only print it — direct exec cannot turn an operand
/// into a command — so denying it is a false positive.
#[cfg(not(windows))]
#[tokio::test]
async fn shell_exec_does_not_mistake_literal_operands_for_commands() {
    let tool = ShellExecTool::with_allowlist(vec!["echo".to_string()]);
    let out = tool
        .invoke(
            &ctx(PathBuf::from(".")),
            json!({ "argv": ["echo", "rm", "-rf", "/tmp/example"] }),
        )
        .await
        .expect("echo printing destructive-looking text is not destructive");

    assert_eq!(out.content["stdout"], "rm -rf /tmp/example\n");
    assert_eq!(out.content["exit_code"], 0);
}

/// The same denylist must still fire when the destructive command really is
/// the binary being executed.
#[cfg(not(windows))]
#[tokio::test]
async fn shell_exec_still_blocks_a_destructive_binary_with_flags() {
    let tool = ShellExecTool::without_allowlist();

    for argv in [
        vec!["rm", "-rf", "/tmp/definitely-not-here"],
        vec!["rm", "-fr", "/tmp/definitely-not-here"],
        vec!["mkfs", "/dev/null"],
        vec!["shutdown", "-h", "now"],
    ] {
        let err = tool
            .invoke(&ctx(PathBuf::from(".")), json!({ "argv": argv }))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "{argv:?} must be denied, got {err:?}"
        );
    }
}

/// P1: an allowlisted binary can emit unbounded output. Capture must be bounded
/// as bytes arrive, not buffered whole and truncated afterwards, so a noisy
/// command cannot exhaust memory before the deadline fires.
#[cfg(not(windows))]
#[tokio::test]
async fn shell_exec_bounds_captured_output() {
    // `yes` emits forever. With an unbounded read this never returns until the
    // timeout, having buffered gigabytes; bounded, it caps and reports it.
    let tool = ShellExecTool::with_allowlist(vec!["yes".to_string()]);
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tool.invoke(
            &ctx(PathBuf::from(".")),
            json!({ "argv": ["yes", "aaaaaaaaaaaaaaaa"], "timeout_secs": 10 }),
        ),
    )
    .await
    .expect("the bounded drain must not hang past the tool deadline");

    match out {
        Ok(output) => {
            let stdout = output.content["stdout"].as_str().unwrap_or_default();
            assert!(
                stdout.len() <= 1024 * 1024,
                "captured stdout must respect the ceiling, got {} bytes",
                stdout.len()
            );
            // Either it was cut short, or the deadline stopped it first.
            assert!(
                output.content["truncated"] == true || output.content["timed_out"] == true,
                "an endless producer must report truncation or timeout: {:?}",
                output.content
            );
        }
        Err(e) => panic!("bounded capture should not error: {e:?}"),
    }
}

/// Normal-sized output is returned intact and not flagged as truncated.
#[cfg(not(windows))]
#[tokio::test]
async fn shell_exec_does_not_flag_small_output_as_truncated() {
    let tool = ShellExecTool::with_allowlist(vec!["echo".to_string()]);
    let out = tool
        .invoke(&ctx(PathBuf::from(".")), json!({ "argv": ["echo", "hi"] }))
        .await
        .expect("runs");

    assert_eq!(out.content["stdout"], "hi\n");
    assert_eq!(out.content["truncated"], false);
    assert_eq!(out.content["timed_out"], false);
}

/// P2: the published schema must encode the exactly-one-input-form rule that
/// `resolve_argv` enforces, so a schema-constrained client cannot generate a
/// request that always fails at invocation.
#[test]
fn shell_exec_schema_encodes_the_exclusive_input_forms() {
    let tool = ShellExecTool::without_allowlist();
    let schema = tool.schema();

    let one_of = schema.input_schema.get("oneOf").expect("schema has oneOf");
    let branches = one_of.as_array().expect("oneOf is an array");
    assert_eq!(branches.len(), 2, "one branch per input form");

    // Each branch requires one form and forbids the other.
    for (required, forbidden) in [("argv", "command"), ("command", "argv")] {
        assert!(
            branches.iter().any(|b| {
                let req = b.get("required").and_then(|r| r.as_array());
                let not_req = b
                    .get("not")
                    .and_then(|n| n.get("required"))
                    .and_then(|r| r.as_array());
                req.is_some_and(|r| r.iter().any(|v| v == required))
                    && not_req.is_some_and(|r| r.iter().any(|v| v == forbidden))
            }),
            "no branch requires {required} while forbidding {forbidden}"
        );
    }

    // The output contract advertises the truncation flag.
    let out_required = schema.output_schema["required"]
        .as_array()
        .expect("output required list");
    assert!(out_required.iter().any(|v| v == "truncated"));
}

/// P2: the inner default deadline must sit below the fused runtime's own
/// per-tool deadline (30s), or the runtime cancels first and the caller never
/// sees the advertised `{ timed_out: true }` result.
#[test]
fn shell_exec_default_timeout_leaves_margin_under_the_runtime_deadline() {
    let tool = ShellExecTool::without_allowlist();
    let described = tool.schema().input_schema["properties"]["timeout_secs"]["description"]
        .as_str()
        .expect("timeout description");

    assert!(
        described.contains("25"),
        "the advertised default must match the implemented one: {described}"
    );
}
