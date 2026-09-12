//! The beads adapter: driving the `bd` CLI as integration tools.
//!
//! Beads is an issue tracker with a CLI. This adapter exposes a **fixed set of
//! verbs** — not an arbitrary command runner that happens to point at `bd`.
//!
//! # Why verbs rather than a passthrough
//!
//! The obvious implementation hands the model a `bd` tool taking an argv array.
//! That is a worse design for two reasons.
//!
//! It cannot be authorised meaningfully: `bd list` and `bd close` would carry
//! the same capability, so granting read access grants write access. Splitting
//! the verbs lets the read path and the write path require *different*
//! capabilities, which is the whole point of having capabilities.
//!
//! And it cannot be audited meaningfully: a receipt saying "ran bd" is not
//! evidence of anything, whereas a receipt naming the verb and its arguments
//! is. Mutations mint receipts; reads do not.
//!
//! # Confinement
//!
//! Every invocation goes through [`ShellExecTool`] with a single-entry
//! allowlist, so the argv-exec confinement from #420 applies unchanged: no
//! shell interpretation, `argv[0]` matched exactly, PATH confined to absolute
//! directories, bounded output, and process-group teardown on timeout. This
//! adapter contributes the *vocabulary*; it deliberately does not contribute
//! new process-spawning machinery, because a second implementation of that is a
//! second thing to get wrong.

use std::sync::Arc;

use ardur_integrations::{AdapterError, Integration, IntegrationAdapter, IntegrationEndpoint};
use ardur_tool_registry::Tool;

mod tools;

pub use tools::{BeadsTool, BeadsVerb};

/// The name this adapter serves: `[integrations.beads]`.
pub const ADAPTER_NAME: &str = "beads";

/// Builds the `beads.*` tools for a declared beads integration.
#[derive(Debug, Default, Clone, Copy)]
pub struct BeadsAdapter;

impl BeadsAdapter {
    /// A new adapter.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl IntegrationAdapter for BeadsAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    fn build(&self, integration: &Integration) -> Result<Vec<Arc<dyn Tool>>, AdapterError> {
        // A directory endpoint cannot be executed. Refusing here rather than
        // failing at first use means the mistake surfaces at boot, when an
        // operator is present to read the message.
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

        // `ShellExecTool::resolve_argv` refuses any whitespace in `argv[0]`, so
        // a path containing a space builds six tools that are denied on every
        // call. The configuration parser deliberately preserves surrounding
        // whitespace (a filename may legitimately contain it), which makes this
        // the right place to notice: fail at boot, where an operator is present
        // to read the reason, rather than at first use with a denial that names
        // the exec layer instead of the configuration.
        if binary.chars().any(char::is_whitespace) {
            return Err(AdapterError::Unusable {
                name: integration.name.to_string(),
                adapter: ADAPTER_NAME.to_string(),
                reason: format!(
                    "the command path `{binary}` contains whitespace, which the \
                     argv-exec allowlist cannot match — every call would be \
                     denied; move the executable somewhere without spaces or \
                     symlink it"
                ),
            });
        }

        Ok(BeadsVerb::ALL
            .iter()
            .map(|verb| {
                Arc::new(BeadsTool::new(
                    *verb,
                    binary.to_string(),
                    integration.capabilities.clone(),
                )) as Arc<dyn Tool>
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_integrations::{AdapterRegistry, parse_integrations};

    #[test]
    fn a_declared_beads_integration_yields_one_tool_per_verb() {
        let set = parse_integrations("[integrations.beads]\ncommand = \"bd\"\nenabled = true\n")
            .expect("fixture parses");
        let registry = AdapterRegistry::new().with(Arc::new(BeadsAdapter::new()));

        let tools = registry.build_active(&set).expect("builds");

        assert_eq!(tools.len(), BeadsVerb::ALL.len());
        let mut ids: Vec<String> = tools.iter().map(|t| t.id().to_string()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "beads.close".to_string(),
                "beads.create".to_string(),
                "beads.list".to_string(),
                "beads.ready".to_string(),
                "beads.show".to_string(),
                "beads.update".to_string(),
            ]
        );
    }

    #[test]
    fn a_directory_endpoint_is_refused_at_build_rather_than_at_first_use() {
        let set = parse_integrations("[integrations.beads]\nroot = \"/vault\"\nenabled = true\n")
            .expect("fixture parses");
        let registry = AdapterRegistry::new().with(Arc::new(BeadsAdapter::new()));

        let err = match registry.build_active(&set) {
            Err(e) => e,
            Ok(_) => panic!("a directory endpoint cannot be executed and must be refused"),
        };
        assert!(
            err.to_string().contains("beads"),
            "the message names the integration: {err}"
        );
    }

    #[test]
    fn a_disabled_beads_integration_yields_nothing() {
        let set =
            parse_integrations("[integrations.beads]\ncommand = \"bd\"\n").expect("fixture parses");
        let registry = AdapterRegistry::new().with(Arc::new(BeadsAdapter::new()));

        assert!(
            registry
                .build_active(&set)
                .expect("disabled is fine")
                .is_empty(),
            "a disabled integration must contribute no tools"
        );
    }
}

#[cfg(test)]
mod whitespace_tests {
    use super::*;
    use ardur_integrations::{AdapterRegistry, parse_integrations};

    /// A command path containing whitespace can never satisfy the argv
    /// allowlist, so building tools for it produces six tools that are denied
    /// on every call. Failing at boot puts the error where an operator can act
    /// on it, and names the configuration rather than the exec layer.
    #[test]
    fn a_command_path_with_whitespace_is_refused_at_build() {
        let set = parse_integrations(
            "[integrations.beads]\ncommand = \"/opt/my tools/bd\"\nenabled = true\n",
        )
        .expect("the parser preserves the path verbatim");

        let registry = AdapterRegistry::new().with(Arc::new(BeadsAdapter::new()));
        let err = match registry.build_active(&set) {
            Err(e) => e,
            Ok(_) => panic!("a path the allowlist cannot match must be refused at build"),
        };

        let message = err.to_string();
        assert!(
            message.contains("whitespace"),
            "the reason must name the actual problem: {message}"
        );
    }
}
