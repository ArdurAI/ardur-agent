//! End-to-end: the obsidian adapter against a real vault on disk.
//!
//! The unit tests assert capability declarations and argument validation. They
//! do not prove that confinement holds — that is a property of the *composed*
//! `file.*` builtins, and the only way to know it survives composition is to
//! point the adapter at a real directory and try to escape it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ardur_integration_obsidian::{ObsidianAdapter, ObsidianTool, ObsidianVerb};
use ardur_integrations::{AdapterRegistry, parse_integrations};
use ardur_tool_registry::{CapTokenRef, InvocationId, SessionId, Tool, ToolContext};
use serde_json::json;

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

/// A vault with one note, plus a secret *outside* it that no verb may reach.
fn vault_with_outside_secret() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(vault.join("notes")).expect("vault");
    std::fs::write(vault.join("notes").join("idea.md"), "# an idea\n").expect("note");
    // Sibling of the vault, not inside it.
    std::fs::write(dir.path().join("secret.txt"), "TOP-SECRET-VALUE").expect("secret");
    (dir, vault)
}

fn tool(verb: ObsidianVerb, vault: &Path) -> ObsidianTool {
    ObsidianTool::new(verb, vault.to_path_buf(), vec![])
}

#[tokio::test]
async fn a_note_inside_the_vault_reads() {
    let (dir, vault) = vault_with_outside_secret();

    let output = tool(ObsidianVerb::Read, &vault)
        .invoke(
            &ctx(dir.path().to_path_buf()),
            json!({ "path": "notes/idea.md" }),
        )
        .await
        .expect("a note inside the vault is readable");

    let rendered = output.content.to_string();
    assert!(
        rendered.contains("an idea"),
        "the note's content must come back: {rendered}"
    );
}

#[tokio::test]
async fn traversal_out_of_the_vault_is_refused() {
    // The security property of this adapter, proven against a real filesystem
    // rather than asserted about the code.
    let (dir, vault) = vault_with_outside_secret();
    let c = ctx(dir.path().to_path_buf());

    for escape in [
        "../secret.txt",
        "notes/../../secret.txt",
        "./../secret.txt",
        "notes/../../../etc/passwd",
    ] {
        let result = tool(ObsidianVerb::Read, &vault)
            .invoke(&c, json!({ "path": escape }))
            .await;

        assert!(
            result.is_err(),
            "`{escape}` escapes the vault and must be refused; got {result:?}"
        );
        if let Ok(output) = result {
            assert!(
                !output.content.to_string().contains("TOP-SECRET-VALUE"),
                "`{escape}` leaked a file outside the vault"
            );
        }
    }
}

#[tokio::test]
async fn an_absolute_path_cannot_escape_the_vault() {
    // An absolute path is the other obvious escape: it must be treated as
    // vault-relative or refused, never followed to the real filesystem root.
    let (dir, vault) = vault_with_outside_secret();
    let secret = dir.path().join("secret.txt");

    let result = tool(ObsidianVerb::Read, &vault)
        .invoke(
            &ctx(dir.path().to_path_buf()),
            json!({ "path": secret.to_string_lossy() }),
        )
        .await;

    match result {
        Err(_) => {}
        Ok(output) => assert!(
            !output.content.to_string().contains("TOP-SECRET-VALUE"),
            "an absolute path must not reach outside the vault"
        ),
    }
}

#[tokio::test]
async fn a_write_lands_inside_the_vault_and_is_recorded() {
    let (dir, vault) = vault_with_outside_secret();

    let output = tool(ObsidianVerb::Write, &vault)
        .invoke(
            &ctx(dir.path().to_path_buf()),
            json!({ "path": "notes/new.md", "content": "written by a test\n" }),
        )
        .await
        .expect("a write inside the vault succeeds");

    let written = std::fs::read_to_string(vault.join("notes").join("new.md"))
        .expect("the note exists on disk");
    assert_eq!(written, "written by a test\n");

    // Structured detail identifies the mutation without carrying the body: a
    // note can be arbitrarily large and arbitrarily sensitive.
    assert_eq!(output.receipt_data["integration"], "obsidian");
    assert_eq!(output.receipt_data["path"], "notes/new.md");
    assert!(
        !output
            .receipt_data
            .to_string()
            .contains("written by a test"),
        "the note body must not be copied into the receipt record: {:?}",
        output.receipt_data
    );
}

#[tokio::test]
async fn a_write_outside_the_vault_is_refused_and_creates_nothing() {
    let (dir, vault) = vault_with_outside_secret();
    let target = dir.path().join("escaped.md");

    let result = tool(ObsidianVerb::Write, &vault)
        .invoke(
            &ctx(dir.path().to_path_buf()),
            json!({ "path": "../escaped.md", "content": "should never exist" }),
        )
        .await;

    assert!(result.is_err(), "a write outside the vault must be refused");
    assert!(
        !target.exists(),
        "a refused write must not create the file: {}",
        target.display()
    );
}

#[tokio::test]
async fn search_defaults_to_the_vault_root() {
    let (dir, vault) = vault_with_outside_secret();

    let output = tool(ObsidianVerb::Search, &vault)
        .invoke(&ctx(dir.path().to_path_buf()), json!({}))
        .await
        .expect("search with no path lists the vault root");

    let rendered = output.content.to_string();
    assert!(
        rendered.contains("notes"),
        "the vault's top level must be listed: {rendered}"
    );
    assert!(
        !rendered.contains("secret.txt"),
        "the vault's sibling must never appear in a listing: {rendered}"
    );
}

#[tokio::test]
async fn the_adapter_builds_three_namespaced_tools() {
    let (_dir, vault) = vault_with_outside_secret();
    let set = parse_integrations(&format!(
        "[integrations.obsidian]\nroot = \"{}\"\nenabled = true\n",
        vault.display()
    ))
    .expect("fixture parses");

    let registry = AdapterRegistry::new().with(Arc::new(ObsidianAdapter::new()));
    let tools = registry.build_active(&set).expect("builds");

    assert_eq!(tools.len(), 3);
    for t in &tools {
        assert!(
            t.id().to_string().starts_with("obsidian."),
            "every tool is namespaced to the integration: {}",
            t.id()
        );
    }
}
