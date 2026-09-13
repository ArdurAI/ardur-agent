//! The dolthub adapter: SQL against a local Dolt clone.
//!
//! Dolt is a SQL database with git-style versioning. This adapter exposes two
//! verbs: a read that runs a single `SELECT`, and a write confined to an
//! operator-declared table allowlist.
//!
//! # Why the read path rejects multiple statements
//!
//! `dolt sql -q` executes *every* statement in its argument, separated by `;`.
//! Verified experimentally rather than assumed: `select 1; insert into t values
//! (99)` returns the select's rows **and** persists the insert.
//!
//! So a read-only guard that inspects the leading verb — "does this start with
//! SELECT?" — is a write hole, because `select 1; delete from notes` passes it.
//! The read path therefore refuses multi-statement input outright rather than
//! trying to classify each statement. A query is one statement or it is
//! rejected.
//!
//! # Confinement
//!
//! Invocation goes through [`ShellExecTool`] with a single-entry allowlist, so
//! the argv-exec confinement from #420 applies: no shell interpretation, exact
//! `argv[0]` match, bounded output, process-group teardown. The SQL text is one
//! argv entry, so a query containing `$(...)` is a string to Dolt rather than
//! syntax to a shell.

use std::sync::Arc;

use ardur_integrations::{AdapterError, Integration, IntegrationAdapter, IntegrationEndpoint};
use ardur_tool_registry::Tool;

mod sql;
mod tools;

pub use sql::{SqlError, ensure_single_read_statement, table_written_by};
pub use tools::{DolthubTool, DolthubVerb};

/// The name this adapter serves: `[integrations.dolthub]`.
pub const ADAPTER_NAME: &str = "dolthub";

/// Builds the `dolthub.*` tools for a declared dolthub integration.
#[derive(Debug, Default, Clone, Copy)]
pub struct DolthubAdapter;

impl DolthubAdapter {
    /// A new adapter.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl IntegrationAdapter for DolthubAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    fn build(&self, integration: &Integration) -> Result<Vec<Arc<dyn Tool>>, AdapterError> {
        // Dolt is driven as a command. A bare directory endpoint would name the
        // clone but not say how to run queries against it.
        let IntegrationEndpoint::Command { binary } = &integration.endpoint else {
            return Err(AdapterError::WrongEndpointKind {
                name: integration.name.to_string(),
                adapter: ADAPTER_NAME.to_string(),
                declared: "a root directory",
                required: "a command",
            });
        };

        let binary = binary.to_str().ok_or_else(|| AdapterError::Unusable {
            name: integration.name.to_string(),
            adapter: ADAPTER_NAME.to_string(),
            reason: "the command path is not valid UTF-8, so it cannot be \
                     matched against an argv allowlist"
                .to_string(),
        })?;

        // Validate against the executor's OWN rules rather than a guess at
        // them. `ShellExecTool::resolve_argv` applies two checks to `argv[0]` —
        // the safe charset AND a whitespace refusal — and `is_safe_exec_char`
        // permits a space, so the charset check alone is not the whole rule.
        // Missing either one registers a tool that doctor reports healthy and
        // that is then denied on every invocation.
        if let Some(bad) = binary
            .chars()
            .find(|c| !ardur_tool_registry::is_safe_exec_char(*c) || c.is_whitespace())
        {
            return Err(AdapterError::Unusable {
                name: integration.name.to_string(),
                adapter: ADAPTER_NAME.to_string(),
                reason: format!(
                    "the command path `{binary}` contains {bad:?}, which the \
                     argv-exec allowlist does not accept — every call would be \
                     denied, so the configuration is refused here instead of \
                     registering a tool that cannot run; move or symlink the \
                     executable to a whitespace-free path of ASCII \
                     alphanumerics and -_./:,=+@%"
                ),
            });
        }

        // Tables the write verb may touch, taken from the operator's
        // `capabilities` list under a `table:` prefix. Absent means **no write
        // tool at all** rather than an unrestricted one: a write path with no
        // declared target is the dangerous default.
        let tables: Vec<String> = integration
            .capabilities
            .iter()
            .filter_map(|c| c.strip_prefix("table:"))
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();

        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(DolthubTool::read(
            binary.to_string(),
            integration.capabilities.clone(),
        ))];
        if !tables.is_empty() {
            tools.push(Arc::new(DolthubTool::write(
                binary.to_string(),
                tables,
                integration.capabilities.clone(),
            )));
        }
        Ok(tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_integrations::{AdapterRegistry, parse_integrations};

    fn tools_for(config: &str) -> Vec<Arc<dyn Tool>> {
        let set = parse_integrations(config).expect("fixture parses");
        AdapterRegistry::new()
            .with(Arc::new(DolthubAdapter::new()))
            .build_active(&set)
            .expect("builds")
    }

    #[test]
    fn without_a_table_allowlist_only_the_read_tool_exists() {
        // The safe default: an integration that declares no writable table gets
        // no write tool, rather than a write tool that may touch anything.
        let tools = tools_for("[integrations.dolthub]\ncommand = \"dolt\"\nenabled = true\n");

        let ids: Vec<String> = tools.iter().map(|t| t.id().to_string()).collect();
        assert_eq!(ids, vec!["dolthub.query".to_string()]);
    }

    #[test]
    fn a_declared_table_adds_the_write_tool() {
        let tools = tools_for(
            "[integrations.dolthub]\ncommand = \"dolt\"\nenabled = true\n\
             capabilities = [\"table:knowledge\"]\n",
        );

        let mut ids: Vec<String> = tools.iter().map(|t| t.id().to_string()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["dolthub.execute".to_string(), "dolthub.query".to_string()]
        );
    }

    #[test]
    fn a_directory_endpoint_is_refused_at_build() {
        let set = parse_integrations("[integrations.dolthub]\nroot = \"/db\"\nenabled = true\n")
            .expect("fixture parses");
        let registry = AdapterRegistry::new().with(Arc::new(DolthubAdapter::new()));

        assert!(
            registry.build_active(&set).is_err(),
            "a directory endpoint does not say how to run queries"
        );
    }

    #[test]
    fn a_disabled_integration_yields_nothing() {
        let set = parse_integrations("[integrations.dolthub]\ncommand = \"dolt\"\n")
            .expect("fixture parses");
        let registry = AdapterRegistry::new().with(Arc::new(DolthubAdapter::new()));

        assert!(registry.build_active(&set).expect("off is fine").is_empty());
    }
}

#[cfg(test)]
mod exec_path_tests {
    use super::*;
    use ardur_integrations::{AdapterRegistry, parse_integrations};

    fn build_result(command: &str) -> Result<Vec<Arc<dyn Tool>>, String> {
        let set = parse_integrations(&format!(
            "[integrations.dolthub]\ncommand = \"{command}\"\nenabled = true\n"
        ))
        .expect("fixture parses");
        AdapterRegistry::new()
            .with(Arc::new(DolthubAdapter::new()))
            .build_active(&set)
            .map_err(|e| e.to_string())
    }

    /// A path the executor would reject must not build.
    ///
    /// A whitespace-only check accepts these, so the tools register, doctor
    /// reports the integration healthy, and every invocation is then denied by
    /// `ShellExecTool::resolve_argv`. Validating against the executor's own
    /// predicate means anything that builds can actually run.
    #[test]
    fn a_path_the_executor_would_deny_is_refused_at_build() {
        for bad in [
            "/opt/dölt",             // non-ASCII
            "/usr/bin/dolt harness", // whitespace
            "C:\\dolt\\dolt.exe",    // backslashes
            "/usr/bin/dolt;rm",      // shell metacharacter
        ] {
            let err = match build_result(bad) {
                Err(e) => e,
                Ok(_) => panic!("a path the executor denies must not produce tools: {bad}"),
            };
            assert!(
                err.contains("argv-exec allowlist does not accept"),
                "the error must explain why `{bad}` is unusable: {err}"
            );
        }
    }

    /// Restrictive direction: ordinary paths still build.
    #[test]
    fn an_ordinary_path_still_builds() {
        for good in ["dolt", "/usr/local/bin/dolt", "/opt/dolt-1.2/bin/dolt"] {
            assert!(
                build_result(good).is_ok(),
                "`{good}` is a legitimate path and must build"
            );
        }
    }
}
