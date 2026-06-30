//! §8.X — integration coverage for filesystem SKILL.md skills: loading a skill
//! tree off disk, invoking the resulting [`SkillTool`] (body + progressive
//! `expand`), and registering it into a [`ToolRegistry`] exposed over MCP.

mod common;

use std::{path::PathBuf, sync::Arc};

use common::{spawn_mcp_server, test_context};

use ardur_tool_registry::{RemoteMcpToolset, SkillLoader, SkillTool, Tool, ToolId, ToolRegistry};
use serde_json::json;
use tempfile::TempDir;

/// Stage a one-skill tree on disk: `<root>/git-commit-message/SKILL.md` plus a
/// `format.md` resource the body references for progressive disclosure.
fn staged_skill_dir() -> TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("git-commit-message");
    std::fs::create_dir_all(&dir).expect("mkdir skill");
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: git-commit-message\ndescription: Draft a Conventional-Commits message.\n---\n\
         Summarize the staged diff. @./format.md\n",
    )
    .expect("write SKILL.md");
    std::fs::write(dir.join("format.md"), "Use `type(scope): subject`.").expect("write resource");
    tmp
}

#[tokio::test]
async fn skill_tool_call_returns_body() {
    let tmp = staged_skill_dir();
    let skills = SkillLoader::load_directory(tmp.path()).expect("load skills");
    assert_eq!(skills.len(), 1);

    let tool = SkillTool::new(skills.into_iter().next().unwrap());
    assert_eq!(tool.id(), ToolId::new("git-commit-message"));
    assert_eq!(
        tool.schema().description,
        "Draft a Conventional-Commits message."
    );

    // No `expand`: the body comes back verbatim, marker un-inlined (lazy).
    let out = tool
        .invoke(&test_context(), json!({}))
        .await
        .expect("invoke");
    assert_eq!(
        out.content,
        json!("Summarize the staged diff. @./format.md")
    );
}

#[tokio::test]
async fn with_expand_inlines_referenced_resource() {
    let tmp = staged_skill_dir();
    let skills = SkillLoader::load_directory(tmp.path()).expect("load skills");
    let tool = SkillTool::new(skills.into_iter().next().unwrap());

    let out = tool
        .invoke(&test_context(), json!({ "expand": ["format.md"] }))
        .await
        .expect("invoke with expand");
    assert_eq!(
        out.content,
        json!("Summarize the staged diff. Use `type(scope): subject`.")
    );
}

#[tokio::test]
async fn registers_via_mcp() {
    let tmp = staged_skill_dir();
    let skills = SkillLoader::load_directory(tmp.path()).expect("load skills");

    let mut registry = ToolRegistry::new();
    for skill in skills {
        registry
            .register(Box::new(SkillTool::new(skill)))
            .expect("register skill tool");
    }

    // The skill tool is visible over the MCP server surface and dispatches.
    let url = spawn_mcp_server(Arc::new(registry)).await;
    let toolset = RemoteMcpToolset::connect(url, None)
        .await
        .expect("connect to MCP server");

    let names = toolset.list_tool_names().await.expect("tools/list");
    assert_eq!(names, vec!["git-commit-message".to_string()]);

    let tools = toolset.into_tools().await.expect("fetch tools");
    let skill = tools
        .iter()
        .find(|t| t.id() == ToolId::new("git-commit-message"))
        .expect("skill tool present");
    let out = skill
        .invoke(&test_context(), json!({}))
        .await
        .expect("skill invocation");
    // The MCP transport returns the structured string content unchanged.
    assert_eq!(
        out.content,
        json!("Summarize the staged diff. @./format.md")
    );
}

#[tokio::test]
async fn repository_examples_include_cite_or_refuse_policy() {
    let examples_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/skills");
    let skills =
        SkillLoader::load_directory(&examples_dir).expect("load repository example skills");
    let skill = skills
        .into_iter()
        .find(|skill| skill.frontmatter.name == "cite-or-refuse")
        .expect("cite-or-refuse example skill is loadable");

    assert!(skill.frontmatter.description.contains("user-corpus spans"));
    assert!(skill.body.contains("refused_empty_retrieval"));
    assert!(skill.body.contains("cited_spans"));
    assert!(skill.body.contains("@./receipt-schema.md"));

    let tool = SkillTool::new(skill);
    let out = tool
        .invoke(&test_context(), json!({ "expand": ["receipt-schema.md"] }))
        .await
        .expect("invoke cite-or-refuse skill with receipt schema");
    let body = out.content.as_str().expect("skill output is a string");
    assert!(body.contains("Grounding receipt requirements"));
    assert!(body.contains("\"mode\": \"cite_or_refuse\""));
    assert!(body.contains("refused_empty_retrieval"));
}
