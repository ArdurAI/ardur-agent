//! The obsidian adapter: reading and writing notes inside a vault.
//!
//! An Obsidian vault is a directory of markdown files. This adapter exposes
//! three verbs — read, search, write — every one of them confined to the
//! configured vault root.
//!
//! # Confinement
//!
//! Path handling is delegated to the `file.*` builtins, which resolve a
//! relative path against a canonicalized root and re-check the result after
//! canonicalization, so `../` traversal and symlink escapes are both refused.
//! Reimplementing that here would mean maintaining a second containment check,
//! and the second one is the one that eventually has the bug.
//!
//! # Capabilities
//!
//! Because this adapter invokes those builtins **directly** rather than
//! dispatching through the runtime, their own `required_capabilities` are never
//! consulted — the dispatcher is what enforces them. So each verb re-declares
//! what its nested execution actually uses (`cap.fs_read`, `cap.fs_write`)
//! alongside its own vault-scoped capability. Without that, an adapter would be
//! a way to reach the filesystem while holding no filesystem capability, which
//! is precisely the control an operator reached for.

use std::sync::Arc;

use ardur_integrations::{AdapterError, Integration, IntegrationAdapter, IntegrationEndpoint};
use ardur_tool_registry::Tool;

mod tools;

pub use tools::{ObsidianTool, ObsidianVerb};

/// The name this adapter serves: `[integrations.obsidian]`.
pub const ADAPTER_NAME: &str = "obsidian";

/// Builds the `obsidian.*` tools for a declared obsidian integration.
#[derive(Debug, Default, Clone, Copy)]
pub struct ObsidianAdapter;

impl ObsidianAdapter {
    /// A new adapter.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl IntegrationAdapter for ObsidianAdapter {
    fn name(&self) -> &str {
        ADAPTER_NAME
    }

    fn build(&self, integration: &Integration) -> Result<Vec<Arc<dyn Tool>>, AdapterError> {
        // A command endpoint has no vault to confine to. Refusing at build
        // surfaces the mistake while an operator is present to read it.
        let IntegrationEndpoint::Directory { root } = &integration.endpoint else {
            return Err(AdapterError::WrongEndpointKind {
                name: integration.name.to_string(),
                adapter: ADAPTER_NAME.to_string(),
                declared: "a command",
                required: "a root directory",
            });
        };

        // The root is NOT checked for existence here. A missing vault is a host
        // fact, not a configuration error — the same configuration is valid on
        // a machine where the vault is mounted, and `ardur doctor` already
        // reports a configured-but-absent directory. Failing the build would
        // make a laptop that has not mounted its vault unable to boot at all.
        Ok(ObsidianVerb::ALL
            .iter()
            .map(|verb| {
                Arc::new(ObsidianTool::new(
                    *verb,
                    root.clone(),
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
    fn a_declared_obsidian_integration_yields_one_tool_per_verb() {
        let set =
            parse_integrations("[integrations.obsidian]\nroot = \"/vault\"\nenabled = true\n")
                .expect("fixture parses");
        let registry = AdapterRegistry::new().with(Arc::new(ObsidianAdapter::new()));

        let tools = registry.build_active(&set).expect("builds");

        let mut ids: Vec<String> = tools.iter().map(|t| t.id().to_string()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "obsidian.read".to_string(),
                "obsidian.search".to_string(),
                "obsidian.write".to_string(),
            ]
        );
    }

    #[test]
    fn a_command_endpoint_is_refused_at_build() {
        let set =
            parse_integrations("[integrations.obsidian]\ncommand = \"obs\"\nenabled = true\n")
                .expect("fixture parses");
        let registry = AdapterRegistry::new().with(Arc::new(ObsidianAdapter::new()));

        let err = match registry.build_active(&set) {
            Err(e) => e,
            Ok(_) => panic!("a command endpoint has no vault and must be refused"),
        };
        assert!(
            err.to_string().contains("obsidian"),
            "the message names the integration: {err}"
        );
    }

    #[test]
    fn a_missing_vault_still_builds_because_it_is_a_host_fact() {
        // An operator may write configuration on a machine where the vault is
        // not mounted. Doctor reports that; the build must not refuse it, or a
        // laptop with an unmounted vault could not start at all.
        let set = parse_integrations(
            "[integrations.obsidian]\nroot = \"/nonexistent/vault\"\nenabled = true\n",
        )
        .expect("fixture parses");
        let registry = AdapterRegistry::new().with(Arc::new(ObsidianAdapter::new()));

        assert_eq!(
            registry.build_active(&set).expect("builds anyway").len(),
            ObsidianVerb::ALL.len()
        );
    }

    #[test]
    fn a_disabled_obsidian_integration_yields_nothing() {
        let set = parse_integrations("[integrations.obsidian]\nroot = \"/vault\"\n")
            .expect("fixture parses");
        let registry = AdapterRegistry::new().with(Arc::new(ObsidianAdapter::new()));

        assert!(
            registry
                .build_active(&set)
                .expect("disabled is fine")
                .is_empty()
        );
    }
}
