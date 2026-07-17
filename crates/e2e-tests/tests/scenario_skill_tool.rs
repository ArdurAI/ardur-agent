//! Scenario §8.X — `skill_tool`.
//!
//! Exercises the filesystem-skill surface end-to-end the way the server boots
//! it:
//!
//! 1. **load + register** — `ardur_server::register_skills` discovers the
//!    repository's shipped `examples/skills/**/SKILL.md` fixtures and registers a
//!    `SkillTool` per skill into the same registry the runtime invokes.
//! 2. **invoke (lazy)** — calling the `git-commit-message` tool returns its
//!    Markdown body with the `@./conventions.md` progressive-disclosure marker
//!    left un-inlined.
//! 3. **invoke (expand)** — passing `expand: ["conventions.md"]` inlines the
//!    referenced resource into the returned body.
//! 4. **serde round-trip** — the `ToolOutput` survives a `serde_json` round-trip
//!    (the public envelope a real caller serializes).

use std::collections::HashMap;
use std::path::PathBuf;

use ardur_tool_registry::{CapTokenRef, InvocationId, SessionId, ToolContext, ToolId, ToolOutput};
use serde_json::json;

/// The repository's shipped example skills directory, resolved from this crate's
/// manifest dir (`crates/e2e-tests` → repo root `examples/skills`).
fn examples_skills_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/skills")
        .canonicalize()
        .expect("examples/skills exists in the repo")
}

fn ctx() -> ToolContext {
    ToolContext {
        cap_token: CapTokenRef(String::new()),
        session_id: SessionId::new(),
        invocation_id: InvocationId::new(),
        cwd: std::env::current_dir().unwrap_or_default(),
        env: HashMap::new(),
        cost_budget_cents: u32::MAX,
    }
}

#[tokio::test]
async fn skill_fixtures_load_register_and_invoke() {
    // (1) Register the shipped skills through the server's public boot helper.
    let mut registry = ardur_server::example_registry("stub", "in-memory");
    ardur_server::register_skills(&mut registry, &[examples_skills_dir()]);

    // Both shipped example skills registered alongside the example tools.
    assert!(registry.get(&ToolId::new("git-commit-message")).is_some());
    assert!(registry.get(&ToolId::new("code-review")).is_some());
    assert!(registry.get(&ToolId::new("echo")).is_some());

    let tool = registry
        .get(&ToolId::new("git-commit-message"))
        .expect("git-commit-message skill registered");
    assert_eq!(
        tool.schema().description,
        "Draft a Conventional-Commits message for a staged diff."
    );

    // (2) Lazy invoke — the resource marker passes through un-inlined.
    let lazy = tool.invoke(&ctx(), json!({})).await.expect("lazy invoke");
    let lazy_body = lazy.content.as_str().expect("body is a string");
    assert!(
        lazy_body.contains("@./conventions.md"),
        "lazy body keeps the resource marker: {lazy_body}"
    );
    assert!(
        !lazy_body.contains("Subject:"),
        "lazy body must not inline the resource"
    );

    // (3) Expand invoke — the referenced resource is inlined.
    let expanded = tool
        .invoke(&ctx(), json!({ "expand": ["conventions.md"] }))
        .await
        .expect("expand invoke");
    let expanded_body = expanded.content.as_str().expect("body is a string");
    assert!(
        !expanded_body.contains("@./conventions.md"),
        "expand replaces the marker"
    );
    assert!(
        expanded_body.contains("Subject:"),
        "expand inlines conventions.md: {expanded_body}"
    );

    // (4) The public output envelope survives a JSON round-trip.
    let bytes = serde_json::to_vec(&expanded).expect("serialize ToolOutput");
    let restored: ToolOutput = serde_json::from_slice(&bytes).expect("deserialize ToolOutput");
    assert_eq!(restored, expanded);
}
