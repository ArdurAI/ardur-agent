use ardur_runtime::SessionId;
use ardur_terminal::{
    BackendKind, DockerBackend, LocalBackend, ModalBackend, SshBackend, TerminalExecTool,
    TerminalPolicy,
};
use ardur_tool_registry::{CapTokenRef, InvocationId, Tool, ToolContext, ToolError};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

fn ctx() -> ToolContext {
    let mut env = HashMap::new();
    env.insert("ARDUR_CEDAR_DECISION".to_string(), "allow".to_string());
    ToolContext {
        cap_token: CapTokenRef("terminal-cap".to_string()),
        session_id: SessionId::new(),
        invocation_id: InvocationId::new(),
        cwd: PathBuf::from("/tmp"),
        env,
        cost_budget_cents: 100,
    }
}

#[tokio::test]
async fn local_backend_executes_real_command_and_mints_receipt() {
    let backend = Arc::new(LocalBackend::new(TerminalPolicy::allow_commands(vec![
        "printf",
    ])));
    let tool = TerminalExecTool::new(backend);

    let output = tool
        .invoke(
            &ctx(),
            json!({"command": "printf ardur", "timeout_secs": 5}),
        )
        .await
        .expect("local command runs");

    assert_eq!(output.content["backend"], "local");
    assert_eq!(output.content["stdout"], "ardur");
    assert_eq!(output.content["exit_code"], 0);
    assert_eq!(output.receipt_data["receipt"]["action"], "terminal.exec");
}

#[tokio::test]
async fn terminal_policy_blocks_disallowed_command_before_execution() {
    let backend = Arc::new(LocalBackend::new(TerminalPolicy::allow_commands(vec![
        "printf",
    ])));
    let tool = TerminalExecTool::new(backend);

    let err = tool
        .invoke(&ctx(), json!({"command": "rm -rf /tmp/ardur-nope"}))
        .await
        .expect_err("policy denies rm");

    assert!(matches!(err, ToolError::Denied { .. }));
}

#[tokio::test]
async fn terminal_cap_token_and_cedar_gates_fail_closed() {
    let backend = Arc::new(LocalBackend::new(TerminalPolicy::allow_commands(vec![
        "printf",
    ])));
    let tool = TerminalExecTool::new(backend);

    let mut missing_cap = ctx();
    missing_cap.cap_token = CapTokenRef(String::new());
    let err = tool
        .invoke(&missing_cap, json!({"command": "printf ardur"}))
        .await
        .expect_err("missing cap-token denied");
    assert!(matches!(err, ToolError::CapabilityDenied(_)));

    let mut cedar = ctx();
    cedar
        .env
        .insert("ARDUR_CEDAR_DECISION".to_string(), "deny".to_string());
    let err = tool
        .invoke(&cedar, json!({"command": "printf ardur"}))
        .await
        .expect_err("Cedar deny denied");
    assert!(matches!(err, ToolError::Denied { .. }));
}

#[tokio::test]
async fn docker_ssh_and_modal_backends_are_capability_gated_and_receipted() {
    let docker = DockerBackend::mock("container-1", TerminalPolicy::allow_commands(vec!["echo"]));
    let ssh = SshBackend::mock(
        "dev.example.com",
        "ardur",
        "SHA256:fixture",
        TerminalPolicy::allow_commands(vec!["uptime"]),
    );
    let modal = ModalBackend::mock(
        "workspace/function",
        TerminalPolicy::allow_commands(vec!["python"]),
    );

    for (backend, command, expected) in [
        (
            Arc::new(docker) as Arc<dyn ardur_terminal::TerminalBackend>,
            "echo hi",
            BackendKind::Docker,
        ),
        (
            Arc::new(ssh) as Arc<dyn ardur_terminal::TerminalBackend>,
            "uptime",
            BackendKind::Ssh,
        ),
        (
            Arc::new(modal) as Arc<dyn ardur_terminal::TerminalBackend>,
            "python -V",
            BackendKind::Modal,
        ),
    ] {
        let tool = TerminalExecTool::new(backend.clone());
        let output = tool
            .invoke(&ctx(), json!({"command": command, "timeout_secs": 5}))
            .await
            .expect("mock platform backend succeeds");
        assert_eq!(backend.kind(), expected);
        assert_eq!(
            output.receipt_data["receipt"]["backend"],
            format!("{expected:?}").to_lowercase()
        );
        assert_eq!(output.receipt_data["policy"]["decision"], "allow");
    }
}

/// ARD-476: a command that tries to break out of an allowlisted binary via a
/// shell operator is rejected at the policy gate — it never reaches a process.
/// Each payload's first token is the allowlisted `printf`; the tail would have
/// executed under the old `/bin/sh -c`.
#[tokio::test]
async fn terminal_rejects_injection_past_allowlisted_binary() {
    let backend = Arc::new(LocalBackend::new(TerminalPolicy::allow_commands(vec![
        "printf",
    ])));
    let tool = TerminalExecTool::new(backend);

    for payload in [
        "printf hi ; rm -rf /tmp/ardur-ard476",
        "printf hi && reboot",
        "printf hi | cat",
        "printf hi > /tmp/ardur-ard476",
        "printf $(reboot)",
        "printf `reboot`",
    ] {
        let err = tool
            .invoke(&ctx(), json!({"command": payload, "timeout_secs": 5}))
            .await
            .expect_err(&format!("injection payload should be denied: {payload:?}"));
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "expected Denied for {payload:?}, got {err:?}"
        );
    }

    // The plain allowlisted command still runs — now via direct exec, no shell.
    let output = tool
        .invoke(
            &ctx(),
            json!({"command": "printf ardursafe", "timeout_secs": 5}),
        )
        .await
        .expect("plain command runs");
    assert_eq!(output.content["stdout"], "ardursafe");
}
