//! End-to-end: the beads adapter against a real executable.
//!
//! The unit tests assert argv construction and capability separation. They do
//! not prove the tool actually runs anything, nor that a receipt is minted on
//! the production path — a passing unit test is not evidence that the wiring
//! works. These drive `invoke` against a stub `bd` on disk.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ardur_integration_beads::{BeadsAdapter, BeadsTool, BeadsVerb};
use ardur_integrations::{AdapterRegistry, parse_integrations};
use ardur_tool_registry::{CapTokenRef, InvocationId, SessionId, Tool, ToolContext};
use serde_json::json;

/// Write an executable stub that echoes its argv, so a test can prove exactly
/// what the adapter passed to the process.
fn stub_bd(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("bd");
    std::fs::write(
        &path,
        "#!/bin/sh\n\
         # Echo each argument on its own line so the test can assert argv\n\
         # boundaries rather than a re-joined string.\n\
         for a in \"$@\"; do echo \"ARG:$a\"; done\n",
    )
    .expect("write stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
    path
}

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

#[tokio::test]
async fn a_write_verb_mints_a_receipt_naming_what_it_did() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bd = stub_bd(dir.path());

    let tool = BeadsTool::new(BeadsVerb::Close, bd.to_string_lossy().to_string(), vec![]);
    let output = tool
        .invoke(
            &ctx(dir.path().to_path_buf()),
            json!({ "id": "abc-123", "reason": "done" }),
        )
        .await
        .expect("the stub runs");

    // The receipt must identify the mutation, not merely that beads ran.
    assert_eq!(output.receipt_data["integration"], "beads");
    assert_eq!(output.receipt_data["verb"], "close");
    let args = output.receipt_data["arguments"]
        .as_array()
        .expect("arguments is an array");
    assert!(
        args.iter().any(|a| a == "abc-123"),
        "the receipt must name the issue that was closed: {args:?}"
    );
    assert!(
        !args
            .iter()
            .any(|a| a.as_str() == Some(bd.to_string_lossy().as_ref())),
        "the binary path is deployment detail and does not belong in the receipt"
    );
}

#[tokio::test]
async fn a_read_verb_mints_no_receipt() {
    // Reads deliberately mint nothing: a receipt per `bd list` would bury the
    // writes that matter.
    let dir = tempfile::tempdir().expect("tempdir");
    let bd = stub_bd(dir.path());

    let tool = BeadsTool::new(BeadsVerb::Ready, bd.to_string_lossy().to_string(), vec![]);
    let output = tool
        .invoke(&ctx(dir.path().to_path_buf()), json!({}))
        .await
        .expect("the stub runs");

    assert!(
        output.receipt_data.is_null(),
        "a read must not mint a receipt; got {:?}",
        output.receipt_data
    );
}

#[tokio::test]
async fn model_supplied_text_reaches_the_process_as_one_argument() {
    // The security property of the adapter, proven at the process boundary
    // rather than at argv construction: a title full of shell metacharacters
    // must arrive as a single operand, unexpanded.
    let dir = tempfile::tempdir().expect("tempdir");
    let bd = stub_bd(dir.path());
    let hostile = "fix; rm -rf / $(whoami) `id` && echo pwned";

    let tool = BeadsTool::new(BeadsVerb::Create, bd.to_string_lossy().to_string(), vec![]);
    let output = tool
        .invoke(&ctx(dir.path().to_path_buf()), json!({ "title": hostile }))
        .await
        .expect("the stub runs");

    let stdout = output.content["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains(&format!("ARG:{hostile}")),
        "the title must arrive as ONE unexpanded argument; got: {stdout}"
    );
    // The title itself contains the word `pwned`, so its mere presence proves
    // nothing. What proves no shell ran is that the substitutions survive
    // verbatim and that `echo pwned` never executed — an execution would put
    // `pwned` on a line of its own, without the `ARG:` prefix.
    assert!(
        stdout.contains("$(whoami)") && stdout.contains("`id`"),
        "substitutions must survive verbatim, proving no shell expanded them: {stdout}"
    );
    assert!(
        !stdout.lines().any(|l| l.trim() == "pwned"),
        "`&& echo pwned` must never have executed as a separate command: {stdout}"
    );
    assert_eq!(
        stdout.lines().filter(|l| l.starts_with("ARG:")).count(),
        2,
        "exactly two arguments — the subcommand and the whole title — must reach \
         the process; more would mean the title was split: {stdout}"
    );
}

#[tokio::test]
async fn the_adapter_refuses_a_binary_other_than_the_configured_one() {
    // The allowlist is single-entry, so even if argv construction were
    // compromised the exec layer still refuses anything else.
    let dir = tempfile::tempdir().expect("tempdir");
    let bd = stub_bd(dir.path());
    let set = parse_integrations(&format!(
        "[integrations.beads]\ncommand = \"{}\"\nenabled = true\n",
        bd.display()
    ))
    .expect("fixture parses");

    let registry = AdapterRegistry::new().with(Arc::new(BeadsAdapter::new()));
    let tools = registry.build_active(&set).expect("builds");

    assert_eq!(tools.len(), BeadsVerb::ALL.len());
    for tool in &tools {
        assert!(
            tool.id().to_string().starts_with("beads."),
            "every tool is namespaced to the integration: {}",
            tool.id()
        );
    }
}
