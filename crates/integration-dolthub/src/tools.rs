//! The `dolthub.*` verbs.

use ardur_tool_registry::{
    Capability, ShellExecTool, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolSchema,
};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::sql::{ensure_single_read_statement, table_written_by};

/// A verb this adapter exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DolthubVerb {
    /// Run one read statement.
    Query,
    /// Run one write statement against an allowlisted table.
    Execute,
}

impl DolthubVerb {
    /// The tool id.
    #[must_use]
    pub fn tool_id(self) -> &'static str {
        match self {
            Self::Query => "dolthub.query",
            Self::Execute => "dolthub.execute",
        }
    }
}

/// One `dolthub.*` verb, backed by the `dolt` CLI.
pub struct DolthubTool {
    verb: DolthubVerb,
    binary: String,
    /// Tables the write verb may target. Empty for the read verb.
    tables: Vec<String>,
    schema: ToolSchema,
    caps: Vec<Capability>,
}

impl DolthubTool {
    /// The read tool.
    #[must_use]
    pub fn read(binary: String, extra_caps: Vec<String>) -> Self {
        Self::build(DolthubVerb::Query, binary, Vec::new(), extra_caps)
    }

    /// The write tool, confined to `tables`.
    #[must_use]
    pub fn write(binary: String, tables: Vec<String>, extra_caps: Vec<String>) -> Self {
        Self::build(DolthubVerb::Execute, binary, tables, extra_caps)
    }

    fn build(
        verb: DolthubVerb,
        binary: String,
        tables: Vec<String>,
        extra_caps: Vec<String>,
    ) -> Self {
        // The lesson from the beads and obsidian adapters: `invoke` runs
        // `ShellExecTool` directly, which bypasses the dispatcher that enforces
        // a tool's `required_capabilities`. Declaring them here is what keeps a
        // deployment's process-spawn gate meaningful.
        let mut caps = vec![
            match verb {
                DolthubVerb::Query => Capability::Custom("integration.dolthub.read".to_string()),
                DolthubVerb::Execute => Capability::Custom("integration.dolthub.write".to_string()),
            },
            Capability::ShellExec,
            Capability::ProcessSpawn,
        ];
        for label in extra_caps {
            // `table:` entries are an allowlist, not capabilities — folding
            // them in would mint `cap.table:knowledge`, which grants nothing
            // and clutters the token.
            if label.starts_with("table:") {
                continue;
            }
            let name = label.strip_prefix("cap.").unwrap_or(&label).to_string();
            let cap = Capability::Custom(name);
            if !caps.contains(&cap) {
                caps.push(cap);
            }
        }

        let description = match verb {
            DolthubVerb::Query => {
                "Run one read-only SQL statement against the Dolt clone and return its rows. \
                 Exactly one statement; writes are refused."
            }
            DolthubVerb::Execute => {
                "Run one write SQL statement (INSERT, UPDATE, DELETE) against an \
                 allowlisted table in the Dolt clone. Exactly one statement; DDL is refused."
            }
        };

        let schema = ToolSchema {
            description: description.to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "Exactly one SQL statement.",
                    },
                },
                "required": ["sql"],
            }),
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
            tables,
            schema,
            caps,
        }
    }

    /// Read and admit the `sql` argument.
    fn admit(&self, args: &Value) -> Result<String, ToolError> {
        // The runtime passes provider-generated arguments to `invoke` without
        // validating them against the published schema, so the type check
        // happens here rather than being assumed.
        let sql = match args.get("sql") {
            Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
            Some(Value::String(_)) | None => {
                return Err(ToolError::InvalidArgs(
                    "`sql` is required and must be a non-empty string".to_string(),
                ));
            }
            Some(_) => {
                return Err(ToolError::InvalidArgs("`sql` must be a string".to_string()));
            }
        };

        match self.verb {
            DolthubVerb::Query => {
                ensure_single_read_statement(&sql).map_err(|e| ToolError::Denied {
                    reason: e.to_string(),
                })?
            }
            DolthubVerb::Execute => {
                table_written_by(&sql, &self.tables).map_err(|e| ToolError::Denied {
                    reason: e.to_string(),
                })?;
            }
        }
        Ok(sql)
    }
}

#[async_trait]
impl Tool for DolthubTool {
    fn id(&self) -> ToolId {
        ToolId::new(self.verb.tool_id())
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let sql = self.admit(&args)?;

        // The SQL is a single argv entry, so a query containing `$(...)` or a
        // pipe is a string to Dolt rather than syntax to a shell. `-r json`
        // gives a machine-readable result the model can work with.
        let argv = vec![
            self.binary.clone(),
            "sql".to_string(),
            "-q".to_string(),
            sql.clone(),
            "-r".to_string(),
            "json".to_string(),
        ];

        let exec = ShellExecTool::with_allowlist(vec![self.binary.clone()]);
        let mut output = exec.invoke(ctx, json!({ "argv": argv })).await?;

        // Structured detail for a mutation. As with the other adapters, the
        // runtime does not yet consume `receipt_data`; this is populated for
        // when it does. The statement is recorded because *which* row changed
        // is the audit question, and a write is already constrained to an
        // allowlisted table.
        if self.verb == DolthubVerb::Execute {
            output.receipt_data = json!({
                "integration": "dolthub",
                "verb": "execute",
                "sql": sql,
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

    fn read_tool() -> DolthubTool {
        DolthubTool::read("dolt".to_string(), vec![])
    }

    fn write_tool() -> DolthubTool {
        DolthubTool::write(
            "dolt".to_string(),
            vec!["knowledge".to_string()],
            vec!["table:knowledge".to_string()],
        )
    }

    #[test]
    fn reads_and_writes_require_different_capabilities() {
        assert_eq!(
            read_tool().required_capabilities()[0].as_str(),
            "cap.integration.dolthub.read"
        );
        assert_eq!(
            write_tool().required_capabilities()[0].as_str(),
            "cap.integration.dolthub.write"
        );
    }

    #[test]
    fn both_verbs_declare_the_capabilities_their_nested_execution_uses() {
        for t in [read_tool(), write_tool()] {
            let labels: Vec<String> = t
                .required_capabilities()
                .iter()
                .map(Capability::as_str)
                .collect();
            assert!(
                labels.contains(&"cap.shell_exec".to_string())
                    && labels.contains(&"cap.process_spawn".to_string()),
                "a tool that spawns `dolt` must declare both: {labels:?}"
            );
        }
    }

    #[test]
    fn a_table_allowlist_entry_is_not_minted_as_a_capability() {
        // `cap.table:knowledge` would grant nothing and clutter the token.
        let labels: Vec<String> = write_tool()
            .required_capabilities()
            .iter()
            .map(Capability::as_str)
            .collect();
        assert!(
            !labels.iter().any(|l| l.contains("table:")),
            "an allowlist entry is not a capability: {labels:?}"
        );
    }

    #[test]
    fn the_read_tool_refuses_a_stacked_write() {
        // The end-to-end expression of the module's central finding.
        let err = read_tool()
            .admit(&json!({ "sql": "select 1; delete from knowledge" }))
            .expect_err("a stacked write must be refused");
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "expected a denial, got {err:?}"
        );
    }

    #[test]
    fn the_read_tool_admits_a_plain_select() {
        assert_eq!(
            read_tool()
                .admit(&json!({ "sql": "select id from knowledge" }))
                .expect("a single select is admitted"),
            "select id from knowledge"
        );
    }

    #[test]
    fn the_write_tool_refuses_a_table_outside_the_allowlist() {
        let err = write_tool()
            .admit(&json!({ "sql": "delete from secrets" }))
            .expect_err("an unlisted table must be refused");
        assert!(matches!(err, ToolError::Denied { .. }));
    }

    #[test]
    fn the_write_tool_admits_an_allowlisted_table() {
        assert!(
            write_tool()
                .admit(&json!({ "sql": "insert into knowledge (id) values ('a')" }))
                .is_ok()
        );
    }

    #[test]
    fn a_non_string_sql_argument_is_refused() {
        for bogus in [
            json!({ "sql": 1 }),
            json!({ "sql": ["select 1"] }),
            json!({}),
        ] {
            assert!(
                read_tool().admit(&bogus).is_err(),
                "a non-string `sql` must be refused: {bogus}"
            );
        }
    }
}
