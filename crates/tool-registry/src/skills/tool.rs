//! [`SkillTool`] — adapts a loaded [`Skill`] to the [`Tool`] contract so a
//! filesystem skill registers and invokes alongside compiled-in and remote MCP
//! tools.
//!
//! Invoking the tool returns the skill's Markdown body. The body may carry
//! `@./resource.md` *progressive-disclosure* markers: by default they pass
//! through verbatim (the model only pays for the body), and the optional
//! `expand` argument inlines named resources on demand.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{Value, json};

use ardur_runtime::CostTuple;

use crate::capability::Capability;
use crate::error::ToolError;
use crate::skills::skill::Skill;
use crate::tool::{Tool, ToolContext, ToolId, ToolOutput, ToolSchema};

/// A [`Tool`] backed by a filesystem [`Skill`].
///
/// The tool's id is the skill's `name`, its description is the skill's
/// `description`, and [`invoke`](Tool::invoke) returns the skill body — with any
/// requested `@./` resources inlined.
///
/// Skills are inert text, so a `SkillTool` declares no
/// [`Capability`]: reading it touches no guarded resource.
pub struct SkillTool {
    id: ToolId,
    schema: ToolSchema,
    body: String,
    dir: PathBuf,
}

impl SkillTool {
    /// Adapt `skill` into a registrable tool.
    #[must_use]
    pub fn new(skill: Skill) -> Self {
        let schema = ToolSchema {
            description: skill.frontmatter.description.clone(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "expand": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Filenames of `@./`-referenced resources to inline into the returned body."
                    }
                }
            }),
            output_schema: json!({ "type": "string" }),
            examples: vec![],
        };
        Self {
            id: ToolId::new(skill.frontmatter.name),
            schema,
            body: skill.body,
            dir: skill.dir,
        }
    }

    /// Render the body, inlining each resource named in `expand`.
    ///
    /// An `@./<file>` marker in the body is replaced in place by the file's
    /// contents; a requested file with no matching marker is appended. Filenames
    /// are resolved against the skill's [`dir`](Skill::dir).
    ///
    /// Path traversal is blocked: filenames containing `..` or absolute paths
    /// (starting with `/`) are rejected.
    fn render(&self, expand: &[String]) -> Result<String, ToolError> {
        let mut rendered = self.body.clone();
        for file in expand {
            let rel = file.trim_start_matches("./");

            // Block path traversal: reject `..` components and absolute paths
            if rel.starts_with('/') || rel.contains("..") {
                return Err(ToolError::ExecutionFailed(format!(
                    "invalid resource path `{rel}`: path traversal blocked"
                )));
            }

            let resolved = self.dir.join(rel);
            // Double-check the resolved path is within the skill directory
            let canonical_dir = std::fs::canonicalize(&self.dir).map_err(|e| {
                ToolError::ExecutionFailed(format!(
                    "cannot canonicalize skill directory: {e}"
                ))
            })?;
            let canonical_resolved = std::fs::canonicalize(&resolved).map_err(|e| {
                ToolError::ExecutionFailed(format!(
                    "cannot inline referenced resource `{rel}`: {e}"
                ))
            })?;
            if !canonical_resolved.starts_with(&canonical_dir) {
                return Err(ToolError::ExecutionFailed(format!(
                    "invalid resource path `{rel}`: path escapes skill directory"
                )));
            }

            let contents = std::fs::read_to_string(&resolved).map_err(|e| {
                ToolError::ExecutionFailed(format!(
                    "cannot inline referenced resource `{rel}`: {e}"
                ))
            })?;
            let marker = format!("@./{rel}");
            if rendered.contains(&marker) {
                rendered = rendered.replace(&marker, &contents);
            } else {
                rendered.push_str("\n\n");
                rendered.push_str(&contents);
            }
        }
        Ok(rendered)
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn id(&self) -> ToolId {
        self.id.clone()
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(&self, _ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let expand: Vec<String> = match args.get("expand") {
            None | Some(Value::Null) => Vec::new(),
            Some(value) => serde_json::from_value(value.clone()).map_err(|e| {
                ToolError::InvalidArgs(format!("`expand` must be an array of strings: {e}"))
            })?,
        };

        let rendered = self.render(&expand)?;
        Ok(ToolOutput {
            content: Value::String(rendered),
            cost: CostTuple::default(),
            receipt_data: json!({ "skill": self.id.0, "expanded": expand }),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ardur_runtime::{CapTokenRef, SessionId};

    use super::*;
    use crate::skills::skill::{Skill, SkillFrontmatter};
    use crate::tool::InvocationId;

    fn ctx() -> ToolContext {
        ToolContext {
            cap_token: CapTokenRef(String::new()),
            session_id: SessionId::new(),
            invocation_id: InvocationId::new(),
            cwd: PathBuf::from("/"),
            env: HashMap::new(),
            cost_budget_cents: 0,
        }
    }

    fn skill(body: &str, dir: PathBuf) -> Skill {
        Skill {
            frontmatter: SkillFrontmatter {
                name: "demo".to_string(),
                description: "A demo skill.".to_string(),
                metadata: Default::default(),
            },
            body: body.to_string(),
            dir,
        }
    }

    #[test]
    fn id_and_description_come_from_frontmatter() {
        let tool = SkillTool::new(skill("body", PathBuf::from(".")));
        assert_eq!(tool.id(), ToolId::new("demo"));
        assert_eq!(tool.schema().description, "A demo skill.");
    }

    #[tokio::test]
    async fn lazy_load_leaves_markers_untouched() {
        // With no `expand`, the `@./details.md` marker passes through verbatim —
        // the referenced resource is not read.
        let tool = SkillTool::new(skill("See @./details.md for more.", PathBuf::from("/nope")));
        let out = tool.invoke(&ctx(), json!({})).await.expect("invoke ok");
        assert_eq!(out.content, json!("See @./details.md for more."));
    }

    #[tokio::test]
    async fn progressive_disclosure_inlines_requested_resource() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("details.md"), "THE DETAILS").unwrap();
        let tool = SkillTool::new(skill("Intro. @./details.md End.", tmp.path().to_path_buf()));

        let out = tool
            .invoke(&ctx(), json!({ "expand": ["details.md"] }))
            .await
            .expect("invoke ok");
        assert_eq!(out.content, json!("Intro. THE DETAILS End."));
        assert_eq!(out.receipt_data["expanded"], json!(["details.md"]));
    }

    #[tokio::test]
    async fn expand_missing_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SkillTool::new(skill("body", tmp.path().to_path_buf()));
        let err = tool
            .invoke(&ctx(), json!({ "expand": ["absent.md"] }))
            .await
            .expect_err("missing resource fails");
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
    }

    #[tokio::test]
    async fn expand_must_be_string_array() {
        let tool = SkillTool::new(skill("body", PathBuf::from(".")));
        let err = tool
            .invoke(&ctx(), json!({ "expand": 42 }))
            .await
            .expect_err("non-array expand fails");
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn expand_path_traversal_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SkillTool::new(skill("body", tmp.path().to_path_buf()));
        let err = tool
            .invoke(&ctx(), json!({ "expand": ["../../../etc/passwd"] }))
            .await
            .expect_err("path traversal blocked");
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
        let msg = format!("{err}");
        assert!(msg.contains("path traversal"), "error mentions path traversal: {msg}");
    }

    #[tokio::test]
    async fn expand_absolute_path_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SkillTool::new(skill("body", tmp.path().to_path_buf()));
        let err = tool
            .invoke(&ctx(), json!({ "expand": ["/etc/passwd"] }))
            .await
            .expect_err("absolute path blocked");
        assert!(matches!(err, ToolError::ExecutionFailed(_)));
        let msg = format!("{err}");
        assert!(msg.contains("path traversal"), "error mentions path traversal: {msg}");
    }
}
