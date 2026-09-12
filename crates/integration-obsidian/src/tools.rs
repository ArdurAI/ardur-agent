//! The `obsidian.*` verbs.
//!
//! Each verb is its own [`Tool`], for the same reason as the beads adapter: a
//! tool is the unit the runtime authorises and receipts, so collapsing read and
//! write behind one tool would make them indistinguishable at authorisation
//! time.

use std::path::PathBuf;

use ardur_tool_registry::{
    Capability, ListDirTool, ReadFileTool, Tool, ToolContext, ToolError, ToolId, ToolOutput,
    ToolSchema, WriteFileTool,
};
use async_trait::async_trait;
use serde_json::{Value, json};

/// A verb this adapter exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObsidianVerb {
    /// Read one note.
    Read,
    /// List the notes under a folder.
    Search,
    /// Create or overwrite a note.
    Write,
}

impl ObsidianVerb {
    /// Every verb this adapter exposes.
    pub const ALL: &'static [Self] = &[Self::Read, Self::Search, Self::Write];

    /// The tool id, e.g. `obsidian.read`.
    #[must_use]
    pub fn tool_id(self) -> &'static str {
        match self {
            Self::Read => "obsidian.read",
            Self::Search => "obsidian.search",
            Self::Write => "obsidian.write",
        }
    }

    /// Whether this verb changes the vault.
    #[must_use]
    pub fn is_write(self) -> bool {
        matches!(self, Self::Write)
    }

    /// The vault-scoped capability a caller must hold.
    ///
    /// Reads and writes are separate, so an agent that may consult notes does
    /// not thereby gain the ability to rewrite them.
    #[must_use]
    pub fn capability(self) -> Capability {
        if self.is_write() {
            Capability::Custom("integration.obsidian.write".to_string())
        } else {
            Capability::Custom("integration.obsidian.read".to_string())
        }
    }

    /// A human-readable description for the model.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Read => "Read one note from the Obsidian vault, by vault-relative path.",
            Self::Search => "List notes under a vault-relative folder.",
            Self::Write => "Create or overwrite a note in the Obsidian vault.",
        }
    }
}

/// One `obsidian.*` verb, confined to a vault root.
pub struct ObsidianTool {
    verb: ObsidianVerb,
    root: PathBuf,
    schema: ToolSchema,
    caps: Vec<Capability>,
}

impl ObsidianTool {
    /// A tool for `verb`, confined to `root`.
    ///
    /// `extra_caps` are the labels the operator listed in
    /// `[integrations.obsidian] capabilities = [...]`; they are added to the
    /// verb's own, never substituted for it.
    #[must_use]
    pub fn new(verb: ObsidianVerb, root: PathBuf, extra_caps: Vec<String>) -> Self {
        // `invoke` runs the `file.*` builtins directly rather than dispatching
        // through the runtime, so their own `required_capabilities` are never
        // checked. Re-declaring them here is what stops this adapter being a
        // route to the filesystem for a caller holding no filesystem
        // capability. A write also reads (the builtins resolve and stat the
        // path), so `FsRead` is declared for every verb.
        let mut caps = vec![verb.capability(), Capability::FsRead];
        if verb.is_write() {
            caps.push(Capability::FsWrite);
        }
        for label in extra_caps {
            let name = label.strip_prefix("cap.").unwrap_or(&label).to_string();
            let cap = Capability::Custom(name);
            if !caps.contains(&cap) {
                caps.push(cap);
            }
        }

        let schema = ToolSchema {
            description: verb.description().to_string(),
            input_schema: Self::input_schema(verb),
            output_schema: json!({ "type": "object" }),
            examples: vec![],
        };

        Self {
            verb,
            root,
            schema,
            caps,
        }
    }

    /// The argument schema for `verb`.
    fn input_schema(verb: ObsidianVerb) -> Value {
        match verb {
            ObsidianVerb::Read => json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Vault-relative path, e.g. `notes/idea.md`.",
                    },
                },
                "required": ["path"],
            }),
            ObsidianVerb::Search => json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Vault-relative folder. Defaults to the vault root.",
                    },
                },
            }),
            ObsidianVerb::Write => json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Vault-relative path, e.g. `notes/idea.md`.",
                    },
                    "content": { "type": "string", "description": "The note's contents." },
                    "mode": {
                        "type": "string",
                        "enum": ["overwrite", "append"],
                        "description": "Default `overwrite`.",
                    },
                },
                "required": ["path", "content"],
            }),
        }
    }

    /// Read a required string argument.
    ///
    /// The runtime hands provider-generated arguments to `invoke` without
    /// checking them against the published schema, so a non-string here must be
    /// refused explicitly rather than falling through to a default.
    fn required_str(args: &Value, name: &str) -> Result<String, ToolError> {
        match args.get(name) {
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
            Some(Value::String(_)) | None => Err(ToolError::InvalidArgs(format!(
                "`{name}` is required and must be a non-empty string"
            ))),
            Some(_) => Err(ToolError::InvalidArgs(format!("`{name}` must be a string"))),
        }
    }
}

#[async_trait]
impl Tool for ObsidianTool {
    fn id(&self) -> ToolId {
        ToolId::new(self.verb.tool_id())
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        // Delegate path resolution to the `file.*` builtins, which canonicalize
        // the root, reject `..` components, and re-check containment after
        // canonicalization so a symlink cannot point out of the vault. A second
        // implementation of that check is a second thing to get wrong.
        let mut output = match self.verb {
            ObsidianVerb::Read => {
                let path = Self::required_str(&args, "path")?;
                ReadFileTool::with_root(self.root.clone())
                    .invoke(ctx, json!({ "path": path }))
                    .await?
            }
            ObsidianVerb::Search => {
                // A missing `path` means the vault root, which is the useful
                // default for "what notes are there".
                let path = match args.get("path") {
                    None | Some(Value::Null) => ".".to_string(),
                    Some(Value::String(s)) if s.trim().is_empty() => ".".to_string(),
                    Some(Value::String(s)) => s.clone(),
                    Some(_) => {
                        return Err(ToolError::InvalidArgs(
                            "`path` must be a string when present".to_string(),
                        ));
                    }
                };
                ListDirTool::with_root(self.root.clone())
                    .invoke(ctx, json!({ "path": path }))
                    .await?
            }
            ObsidianVerb::Write => {
                let path = Self::required_str(&args, "path")?;
                let content = Self::required_str(&args, "content")?;
                // Constrained rather than passed through: an unrecognised mode
                // would reach the builtin as an unknown string.
                let mode = match args.get("mode") {
                    None | Some(Value::Null) => "overwrite".to_string(),
                    Some(Value::String(m)) if m == "overwrite" || m == "append" => m.clone(),
                    Some(Value::String(m)) => {
                        return Err(ToolError::InvalidArgs(format!(
                            "`mode` must be `overwrite` or `append`; got `{m}`"
                        )));
                    }
                    Some(_) => {
                        return Err(ToolError::InvalidArgs(
                            "`mode` must be a string when present".to_string(),
                        ));
                    }
                };
                WriteFileTool::with_root(self.root.clone())
                    .invoke(
                        ctx,
                        json!({ "path": path, "content": content, "mode": mode }),
                    )
                    .await?
            }
        };

        // Structured detail for a mutation.
        //
        // Honest limitation, as with the beads adapter: the runtime does not
        // consume `receipt_data` — it receipts every tool call uniformly from
        // the call name and digests. This is populated for when that changes,
        // not as a distinction that exists today. Note the *content* is
        // deliberately excluded: a note's body can be arbitrarily large and
        // arbitrarily sensitive, and the path plus mode is what identifies the
        // mutation.
        if self.verb.is_write() {
            output.receipt_data = json!({
                "integration": "obsidian",
                "verb": "write",
                "path": args.get("path").and_then(Value::as_str).unwrap_or_default(),
                "mode": args.get("mode").and_then(Value::as_str).unwrap_or("overwrite"),
            });
        }
        Ok(output)
    }

    fn required_capabilities(&self) -> &[Capability] {
        &self.caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(verb: ObsidianVerb) -> ObsidianTool {
        ObsidianTool::new(verb, PathBuf::from("/vault"), vec![])
    }

    #[test]
    fn reads_and_writes_require_different_vault_capabilities() {
        let read = tool(ObsidianVerb::Read);
        let write = tool(ObsidianVerb::Write);

        assert_eq!(
            read.required_capabilities()[0].as_str(),
            "cap.integration.obsidian.read"
        );
        assert_eq!(
            write.required_capabilities()[0].as_str(),
            "cap.integration.obsidian.write"
        );
    }

    #[test]
    fn every_verb_declares_the_filesystem_capabilities_its_nested_call_uses() {
        // The lesson from the beads adapter: composing a Tool inside a Tool
        // inherits its behaviour but NOT its authorisation, because the
        // dispatcher is what enforces `required_capabilities`. Without these,
        // this adapter reaches the filesystem for a caller holding no
        // filesystem capability.
        for verb in ObsidianVerb::ALL {
            let t = tool(*verb);
            let labels: Vec<String> = t
                .required_capabilities()
                .iter()
                .map(Capability::as_str)
                .collect();

            assert!(
                labels.contains(&"cap.fs_read".to_string()),
                "{} resolves paths through a file builtin and must declare \
                 cap.fs_read: {labels:?}",
                verb.tool_id()
            );
            if verb.is_write() {
                assert!(
                    labels.contains(&"cap.fs_write".to_string()),
                    "{} writes and must declare cap.fs_write: {labels:?}",
                    verb.tool_id()
                );
            } else {
                assert!(
                    !labels.contains(&"cap.fs_write".to_string()),
                    "{} only reads and must NOT demand cap.fs_write: {labels:?}",
                    verb.tool_id()
                );
            }
        }
    }

    #[test]
    fn operator_capabilities_are_added_never_substituted() {
        let t = ObsidianTool::new(
            ObsidianVerb::Write,
            PathBuf::from("/vault"),
            vec!["cap.integration.obsidian.extra".to_string()],
        );
        let labels: Vec<String> = t
            .required_capabilities()
            .iter()
            .map(Capability::as_str)
            .collect();

        assert!(labels.contains(&"cap.integration.obsidian.write".to_string()));
        assert!(labels.contains(&"cap.fs_write".to_string()));
        assert!(labels.contains(&"cap.integration.obsidian.extra".to_string()));
    }

    #[test]
    fn a_non_string_argument_is_refused_rather_than_defaulted() {
        // The runtime does not validate provider-generated arguments against
        // the published schema, so a tool cannot trust its own schema.
        assert!(ObsidianTool::required_str(&json!({ "path": 7 }), "path").is_err());
        assert!(ObsidianTool::required_str(&json!({ "path": {} }), "path").is_err());
        assert!(ObsidianTool::required_str(&json!({ "path": "  " }), "path").is_err());
        assert!(ObsidianTool::required_str(&json!({}), "path").is_err());
        assert_eq!(
            ObsidianTool::required_str(&json!({ "path": "a.md" }), "path").expect("valid"),
            "a.md"
        );
    }
}
