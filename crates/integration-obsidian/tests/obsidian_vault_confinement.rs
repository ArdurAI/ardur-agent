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

/// A dangling symlink must not be a way out of the vault.
///
/// This was a real escape, found in review and reproduced before it was fixed:
/// `canonicalize` resolves a symlink and *then* fails when the target does not
/// exist, which is indistinguishable from "this path does not exist yet". The
/// containment check walked up to the link's in-vault parent, approved it, and
/// the write followed the link — creating the file outside the vault.
///
/// Every earlier confinement test used paths whose targets existed, which is
/// exactly why none of them caught it.
#[cfg(unix)]
#[tokio::test]
async fn a_dangling_symlink_cannot_write_outside_the_vault() {
    let (dir, vault) = vault_with_outside_secret();
    let outside = dir.path().join("ESCAPED.md");
    assert!(!outside.exists(), "precondition: the target does not exist");

    // A link inside the vault aimed at a non-existent path outside it.
    std::os::unix::fs::symlink(&outside, vault.join("link.md")).expect("symlink");

    let result = tool(ObsidianVerb::Write, &vault)
        .invoke(
            &ctx(dir.path().to_path_buf()),
            json!({ "path": "link.md", "content": "PWNED" }),
        )
        .await;

    assert!(
        result.is_err(),
        "writing through a dangling symlink must be refused; got {result:?}"
    );
    assert!(
        !outside.exists(),
        "the write must not have created {} outside the vault",
        outside.display()
    );
}

/// The same hole, reading rather than writing, and with a target that exists.
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_to_an_existing_outside_file_cannot_be_read() {
    let (dir, vault) = vault_with_outside_secret();
    std::os::unix::fs::symlink(dir.path().join("secret.txt"), vault.join("leak.md"))
        .expect("symlink");

    let result = tool(ObsidianVerb::Read, &vault)
        .invoke(&ctx(dir.path().to_path_buf()), json!({ "path": "leak.md" }))
        .await;

    match result {
        Err(_) => {}
        Ok(output) => assert!(
            !output.content.to_string().contains("TOP-SECRET-VALUE"),
            "a symlink must not leak a file outside the vault"
        ),
    }
}

/// A symlink that stays inside the vault is still usable.
///
/// The contrast that keeps the two tests above honest: the fix must reject
/// escaping links, not all links.
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_pointing_inside_the_vault_still_works() {
    let (dir, vault) = vault_with_outside_secret();
    std::os::unix::fs::symlink(vault.join("notes").join("idea.md"), vault.join("alias.md"))
        .expect("symlink");

    let output = tool(ObsidianVerb::Read, &vault)
        .invoke(
            &ctx(dir.path().to_path_buf()),
            json!({ "path": "alias.md" }),
        )
        .await
        .expect("a link wholly inside the vault is legitimate");

    assert!(
        output.content.to_string().contains("an idea"),
        "an in-vault symlink must resolve normally: {:?}",
        output.content
    );
}

/// An empty note is a legitimate thing to write.
#[tokio::test]
async fn an_empty_note_can_be_written() {
    let (dir, vault) = vault_with_outside_secret();

    tool(ObsidianVerb::Write, &vault)
        .invoke(
            &ctx(dir.path().to_path_buf()),
            json!({ "path": "notes/blank.md", "content": "" }),
        )
        .await
        .expect("creating a blank note is an ordinary operation");

    let written =
        std::fs::read_to_string(vault.join("notes").join("blank.md")).expect("the note exists");
    assert_eq!(written, "", "the note must be empty, not rejected");
}
