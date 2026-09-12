//! The `beads.*` verbs.
//!
//! Each verb is its own [`Tool`], because a tool is the unit the runtime
//! authorises, gates on cost, and receipts. Collapsing them into one tool with
//! a `verb` argument would make `beads.close` indistinguishable from
//! `beads.list` at authorisation time.

use ardur_tool_registry::{
    Capability, ShellExecTool, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolSchema,
};
use async_trait::async_trait;
use serde_json::{Value, json};

/// How a verb changes the tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutability {
    /// Observes only.
    Read,
    /// Changes tracker state, and therefore mints a receipt.
    Write,
}

/// A `bd` subcommand this adapter exposes.
///
/// A closed set, not a passthrough: an argv the model composes freely could
/// reach `bd` subcommands nobody reviewed, and every one of them would carry
/// whichever capability the single tool declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeadsVerb {
    /// Issues with no unmet dependencies.
    Ready,
    /// All issues, optionally filtered by status.
    List,
    /// One issue in full.
    Show,
    /// Create an issue.
    Create,
    /// Append notes to an issue.
    Update,
    /// Close an issue with a reason.
    Close,
}

impl BeadsVerb {
    /// Every verb this adapter exposes.
    pub const ALL: &'static [Self] = &[
        Self::Ready,
        Self::List,
        Self::Show,
        Self::Create,
        Self::Update,
        Self::Close,
    ];

    /// The tool id, e.g. `beads.ready`.
    #[must_use]
    pub fn tool_id(self) -> &'static str {
        match self {
            Self::Ready => "beads.ready",
            Self::List => "beads.list",
            Self::Show => "beads.show",
            Self::Create => "beads.create",
            Self::Update => "beads.update",
            Self::Close => "beads.close",
        }
    }

    /// The `bd` subcommand.
    #[must_use]
    pub fn subcommand(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::List => "list",
            Self::Show => "show",
            Self::Create => "create",
            Self::Update => "update",
            Self::Close => "close",
        }
    }

    /// Whether this verb changes tracker state.
    #[must_use]
    pub fn mutability(self) -> Mutability {
        match self {
            Self::Ready | Self::List | Self::Show => Mutability::Read,
            Self::Create | Self::Update | Self::Close => Mutability::Write,
        }
    }

    /// The capability label a caller must hold.
    ///
    /// Reads and writes are **separate** capabilities, so a grant that lets an
    /// agent consult the tracker does not also let it close issues.
    #[must_use]
    pub fn capability(self) -> Capability {
        match self.mutability() {
            Mutability::Read => Capability::Custom("integration.beads.read".to_string()),
            Mutability::Write => Capability::Custom("integration.beads.write".to_string()),
        }
    }

    /// A human-readable description for the model.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Ready => "List beads issues that are ready to work on (no unmet dependencies).",
            Self::List => "List beads issues, optionally filtered by status.",
            Self::Show => "Show one beads issue in full, by id.",
            Self::Create => "Create a beads issue. Mints a receipt.",
            Self::Update => "Append notes to a beads issue. Mints a receipt.",
            Self::Close => "Close a beads issue with a reason. Mints a receipt.",
        }
    }
}

/// One `beads.*` verb, backed by the `bd` CLI.
pub struct BeadsTool {
    verb: BeadsVerb,
    binary: String,
    schema: ToolSchema,
    caps: Vec<Capability>,
}

impl BeadsTool {
    /// A tool for `verb`, executing `binary`.
    ///
    /// `extra_caps` are the labels the operator listed in
    /// `[integrations.beads] capabilities = [...]`. They are *added* to the
    /// verb's own read/write capability, never substituted for it: an operator
    /// tightening access must not be able to accidentally widen it.
    #[must_use]
    pub fn new(verb: BeadsVerb, binary: String, extra_caps: Vec<String>) -> Self {
        let mut caps = vec![verb.capability()];
        for label in extra_caps {
            // Labels arrive as `cap.foo` or `foo`; `Capability::Custom` renders
            // `cap.{name}`, so strip a leading `cap.` to avoid `cap.cap.foo`.
            let name = label.strip_prefix("cap.").unwrap_or(&label).to_string();
            let cap = Capability::Custom(name);
            if !caps.contains(&cap) {
                caps.push(cap);
            }
        }

        let schema = ToolSchema {
            description: verb.description().to_string(),
            input_schema: Self::input_schema(verb),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "stdout": { "type": "string" },
                    "stderr": { "type": "string" },
                    "exit_code": { "type": "integer" },
                },
            }),
            examples: vec![],
        };

        Self {
            verb,
            binary,
            schema,
            caps,
        }
    }

    /// The argument schema for `verb`.
    ///
    /// Each verb takes only the arguments it needs, so the model cannot pass an
    /// `id` to `ready` and have it silently ignored — or, worse, appended to
    /// the argv as an unexpected operand.
    fn input_schema(verb: BeadsVerb) -> Value {
        match verb {
            BeadsVerb::Ready => json!({ "type": "object", "properties": {} }),
            BeadsVerb::List => json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "Filter by status, e.g. `open` or `closed`.",
                        "enum": ["open", "closed", "in_progress"],
                    },
                },
            }),
            BeadsVerb::Show => json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The issue id." },
                },
                "required": ["id"],
            }),
            BeadsVerb::Create => json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "The issue title." },
                },
                "required": ["title"],
            }),
            BeadsVerb::Update => json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The issue id." },
                    "notes": { "type": "string", "description": "Notes to append." },
                },
                "required": ["id", "notes"],
            }),
            BeadsVerb::Close => json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The issue id." },
                    "reason": { "type": "string", "description": "Why it is closed." },
                },
                "required": ["id", "reason"],
            }),
        }
    }

    /// Build the argv for this verb from validated arguments.
    ///
    /// Arguments are placed as **separate argv entries**, never interpolated
    /// into a command string, so a title containing `;` or `$(...)` is one
    /// operand rather than shell syntax. `ShellExecTool` does not interpret a
    /// shell either, which makes this belt and braces — deliberately, since
    /// this is the boundary where model-supplied text meets a process.
    fn argv(&self, args: &Value) -> Result<Vec<String>, ToolError> {
        let field = |name: &str| -> Result<String, ToolError> {
            args.get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "`{name}` is required and must be a non-empty string"
                    ))
                })
        };

        let mut argv = vec![self.binary.clone(), self.verb.subcommand().to_string()];
        match self.verb {
            BeadsVerb::Ready => {}
            BeadsVerb::List => {
                if let Some(status) = args.get("status").and_then(Value::as_str) {
                    // Constrained rather than passed through: an arbitrary
                    // string here would become an operand `bd list` may read as
                    // a flag.
                    const ALLOWED: &[&str] = &["open", "closed", "in_progress"];
                    if !ALLOWED.contains(&status) {
                        return Err(ToolError::InvalidArgs(format!(
                            "`status` must be one of {}; got `{status}`",
                            ALLOWED.join(", ")
                        )));
                    }
                    argv.push("--status".to_string());
                    argv.push(status.to_string());
                }
            }
            BeadsVerb::Show => argv.push(field("id")?),
            BeadsVerb::Create => argv.push(field("title")?),
            BeadsVerb::Update => {
                argv.push(field("id")?);
                argv.push("--append-notes".to_string());
                argv.push(field("notes")?);
            }
            BeadsVerb::Close => {
                argv.push(field("id")?);
                argv.push("--reason".to_string());
                argv.push(field("reason")?);
            }
        }
        Ok(argv)
    }
}

#[async_trait]
impl Tool for BeadsTool {
    fn id(&self) -> ToolId {
        ToolId::new(self.verb.tool_id())
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let argv = self.argv(&args)?;

        // Delegate to the argv-exec confinement rather than spawning here: no
        // shell interpretation, exact `argv[0]` match, confined PATH, bounded
        // output, and process-group teardown on timeout all come for free, and
        // stay correct as that code improves.
        let exec = ShellExecTool::with_allowlist(vec![self.binary.clone()]);
        let mut output = exec.invoke(ctx, json!({ "argv": argv })).await?;

        // Receipts are the audit trail, so a mutation records what it did.
        // Reads deliberately mint nothing: a receipt per `bd list` would bury
        // the writes that matter in noise.
        output.receipt_data = match self.verb.mutability() {
            Mutability::Read => Value::Null,
            Mutability::Write => json!({
                "integration": "beads",
                "verb": self.verb.subcommand(),
                // The argv minus the binary path: the binary is deployment
                // detail, while the verb and its operands are what was done.
                "arguments": argv[1..],
            }),
        };
        Ok(output)
    }

    fn required_capabilities(&self) -> &[Capability] {
        &self.caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(verb: BeadsVerb) -> BeadsTool {
        BeadsTool::new(verb, "bd".to_string(), vec![])
    }

    #[test]
    fn reads_and_writes_require_different_capabilities() {
        // The property that makes per-verb tools worth the extra surface: a
        // grant permitting the tracker to be consulted must not permit issues
        // to be closed.
        let read = tool(BeadsVerb::List);
        let write = tool(BeadsVerb::Close);

        assert_eq!(
            read.required_capabilities()[0].as_str(),
            "cap.integration.beads.read"
        );
        assert_eq!(
            write.required_capabilities()[0].as_str(),
            "cap.integration.beads.write"
        );
        assert_ne!(
            read.required_capabilities()[0],
            write.required_capabilities()[0]
        );
    }

    #[test]
    fn operator_capabilities_are_added_never_substituted() {
        // An operator narrowing access must not be able to widen it: the
        // verb's own capability survives whatever they list.
        let t = BeadsTool::new(
            BeadsVerb::Close,
            "bd".to_string(),
            vec!["cap.integration.beads.extra".to_string()],
        );
        let labels: Vec<String> = t
            .required_capabilities()
            .iter()
            .map(|c| c.as_str())
            .collect();

        assert!(
            labels.contains(&"cap.integration.beads.write".to_string()),
            "the verb's own capability must survive: {labels:?}"
        );
        assert!(labels.contains(&"cap.integration.beads.extra".to_string()));
    }

    #[test]
    fn a_cap_prefixed_label_is_not_double_prefixed() {
        let t = BeadsTool::new(
            BeadsVerb::List,
            "bd".to_string(),
            vec!["cap.custom.thing".to_string()],
        );
        let labels: Vec<String> = t
            .required_capabilities()
            .iter()
            .map(|c| c.as_str())
            .collect();
        assert!(
            labels.contains(&"cap.custom.thing".to_string()),
            "`cap.` must not be doubled: {labels:?}"
        );
    }

    #[test]
    fn arguments_become_separate_argv_entries_not_shell_syntax() {
        // The boundary where model-supplied text meets a process. A title
        // containing shell metacharacters must be one operand.
        let t = tool(BeadsVerb::Create);
        let argv = t
            .argv(&json!({ "title": "fix; rm -rf / $(whoami)" }))
            .expect("a title is a title, however it is spelled");

        assert_eq!(
            argv,
            vec![
                "bd".to_string(),
                "create".to_string(),
                "fix; rm -rf / $(whoami)".to_string()
            ],
            "the whole title must be ONE argv entry, never split or interpreted"
        );
    }

    #[test]
    fn a_missing_or_blank_required_argument_is_refused() {
        let t = tool(BeadsVerb::Show);
        assert!(t.argv(&json!({})).is_err(), "a missing id must be refused");
        assert!(
            t.argv(&json!({ "id": "   " })).is_err(),
            "a blank id would become an empty operand"
        );
    }

    #[test]
    fn an_unexpected_status_filter_is_refused_rather_than_passed_through() {
        // An arbitrary string here would reach `bd list` as an operand it may
        // read as a flag.
        let t = tool(BeadsVerb::List);
        assert!(t.argv(&json!({ "status": "--help" })).is_err());
        assert_eq!(
            t.argv(&json!({ "status": "open" })).expect("allowed"),
            vec![
                "bd".to_string(),
                "list".to_string(),
                "--status".to_string(),
                "open".to_string()
            ]
        );
    }

    #[test]
    fn every_verb_maps_to_its_own_tool_id_and_subcommand() {
        let ids: Vec<&str> = BeadsVerb::ALL.iter().map(|v| v.tool_id()).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "tool ids must be unique: {ids:?}");

        // Writes are exactly the three mutating verbs.
        let writes: Vec<&str> = BeadsVerb::ALL
            .iter()
            .filter(|v| v.mutability() == Mutability::Write)
            .map(|v| v.subcommand())
            .collect();
        assert_eq!(writes, vec!["create", "update", "close"]);
    }
}
