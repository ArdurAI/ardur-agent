//! The adapter registry: turning declared integrations into live tools.
//!
//! An [`IntegrationAdapter`] knows how to build tools for one *kind* of
//! integration. The registry holds the adapters a binary was compiled with,
//! matches them against what an operator declared, and produces the tools for
//! the enabled ones.
//!
//! # Why a registry rather than direct construction
//!
//! The set of adapters is fixed at compile time, but the set of *enabled*
//! integrations is not known until configuration loads. The registry is the
//! join between the two, and it is the single place where "enabled" turns into
//! "callable" — which makes it the single place to audit for the off-by-default
//! posture, instead of that posture living in each adapter's constructor.

use std::collections::BTreeMap;
use std::sync::Arc;

use ardur_tool_registry::Tool;

use crate::{Integration, IntegrationName, IntegrationSet};

/// Builds tools for one kind of integration.
///
/// Implementors live in their own crates (the beads, dolthub and obsidian
/// adapters); this crate owns only the contract, so that adding an adapter
/// cannot change how configuration is interpreted.
pub trait IntegrationAdapter: Send + Sync {
    /// The integration name this adapter serves, e.g. `beads`.
    ///
    /// Matching is by name rather than by endpoint shape because two
    /// integrations can share a shape — `beads` and some other CLI are both
    /// `Command` — while needing entirely different tools.
    fn name(&self) -> &str;

    /// Build this integration's tools from its configuration.
    ///
    /// Called only for an integration that is both declared and enabled, so an
    /// implementation never has to re-check the posture. Returning an empty
    /// vector is legitimate: an adapter may decide its configuration yields no
    /// usable tools.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] if the integration's configuration is valid
    /// TOML but unusable for this specific adapter — a wrong endpoint kind, or
    /// a missing required capability.
    fn build(&self, integration: &Integration) -> Result<Vec<Arc<dyn Tool>>, AdapterError>;
}

/// Why an adapter refused to build.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    /// The declared endpoint is not the kind this adapter drives.
    #[error(
        "integration `{name}` is declared with {declared}, but the `{adapter}` \
         adapter drives {required}"
    )]
    WrongEndpointKind {
        /// The integration name.
        name: String,
        /// The adapter that refused.
        adapter: String,
        /// What was declared.
        declared: &'static str,
        /// What the adapter needs.
        required: &'static str,
    },
    /// The adapter could not build for a reason of its own.
    #[error("integration `{name}` could not be built by the `{adapter}` adapter: {reason}")]
    Unusable {
        /// The integration name.
        name: String,
        /// The adapter that refused.
        adapter: String,
        /// The adapter's explanation.
        reason: String,
    },
}

/// Why wiring a whole set failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// An integration was enabled but no compiled-in adapter serves it.
    ///
    /// Fails the boot rather than warning, because the operator asked for a
    /// capability the binary cannot provide: continuing would run a deployment
    /// that silently lacks something its configuration says it has.
    #[error(
        "integration `{name}` is enabled but this binary has no adapter for it \
         (known adapters: {known}); refusing to boot rather than running \
         without a capability the configuration declares"
    )]
    NoAdapter {
        /// The unmatched integration.
        name: String,
        /// The adapters this binary carries.
        known: String,
    },
    /// An adapter refused to build.
    #[error(transparent)]
    Adapter(#[from] AdapterError),
}

/// The adapters a binary carries.
#[derive(Default)]
pub struct AdapterRegistry {
    adapters: BTreeMap<String, Arc<dyn IntegrationAdapter>>,
}

impl AdapterRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an adapter, replacing any earlier one of the same name.
    #[must_use]
    pub fn with(mut self, adapter: Arc<dyn IntegrationAdapter>) -> Self {
        self.adapters.insert(adapter.name().to_string(), adapter);
        self
    }

    /// The registered adapter names, in order.
    #[must_use]
    pub fn known(&self) -> Vec<&str> {
        self.adapters.keys().map(String::as_str).collect()
    }

    /// Build every tool contributed by the **enabled** integrations in `set`.
    ///
    /// Disabled and undeclared integrations contribute nothing, and no adapter
    /// is consulted for them — so a disabled integration cannot execute adapter
    /// code at all, rather than executing it and being filtered afterwards.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] if an enabled integration has no adapter, or
    /// if an adapter refuses to build.
    pub fn build_active(&self, set: &IntegrationSet) -> Result<Vec<Arc<dyn Tool>>, RegistryError> {
        let mut tools = Vec::new();
        for integration in set.active() {
            let adapter = self
                .adapters
                .get(integration.name.as_str())
                .ok_or_else(|| RegistryError::NoAdapter {
                    name: integration.name.to_string(),
                    known: if self.adapters.is_empty() {
                        "none".to_string()
                    } else {
                        self.known().join(", ")
                    },
                })?;
            tools.extend(adapter.build(integration)?);
        }
        Ok(tools)
    }

    /// Build the tools for one integration, whether or not it is enabled.
    ///
    /// Used by doctor to answer "would this configuration actually work?"
    /// without booting: an adapter existing by name does not mean it accepts
    /// this endpoint, and only the adapter can say. `build_active` is the
    /// runtime path; this is the same question asked of a single entry.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::NoAdapter`] if nothing serves this name, or the
    /// adapter's own refusal.
    pub fn build_one(
        &self,
        integration: &Integration,
    ) -> Result<Vec<Arc<dyn Tool>>, RegistryError> {
        let adapter = self
            .adapters
            .get(integration.name.as_str())
            .ok_or_else(|| RegistryError::NoAdapter {
                name: integration.name.to_string(),
                known: if self.adapters.is_empty() {
                    "none".to_string()
                } else {
                    self.known().join(", ")
                },
            })?;
        Ok(adapter.build(integration)?)
    }

    /// Whether an adapter exists for `name`. Used by doctor to distinguish
    /// "not configured" from "configured but unsupported by this build".
    #[must_use]
    pub fn has(&self, name: &IntegrationName) -> bool {
        self.adapters.contains_key(name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IntegrationEndpoint, parse_integrations};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts how often it is asked to build, so a test can prove an adapter
    /// was never consulted rather than merely producing nothing.
    struct CountingAdapter {
        name: String,
        calls: Arc<AtomicUsize>,
    }

    impl IntegrationAdapter for CountingAdapter {
        fn name(&self) -> &str {
            &self.name
        }

        fn build(&self, integration: &Integration) -> Result<Vec<Arc<dyn Tool>>, AdapterError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match integration.endpoint {
                IntegrationEndpoint::Command { .. } => Ok(Vec::new()),
                IntegrationEndpoint::Directory { .. } => Err(AdapterError::WrongEndpointKind {
                    name: integration.name.to_string(),
                    adapter: self.name.clone(),
                    declared: "a root directory",
                    required: "a command",
                }),
            }
        }
    }

    fn counting(name: &str) -> (Arc<CountingAdapter>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(CountingAdapter {
                name: name.to_string(),
                calls: Arc::clone(&calls),
            }),
            calls,
        )
    }

    #[test]
    fn a_disabled_integration_never_reaches_its_adapter() {
        // Stronger than "contributes no tools": the adapter is not executed at
        // all, so a disabled integration cannot run adapter code that touches
        // the filesystem or spawns a process.
        let (adapter, calls) = counting("beads");
        let registry = AdapterRegistry::new().with(adapter);
        let set = parse_integrations("[integrations.beads]\ncommand = \"bd\"\n").unwrap();

        let tools = registry
            .build_active(&set)
            .expect("disabled is not an error");

        assert!(tools.is_empty());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a disabled integration must not consult its adapter at all"
        );
    }

    #[test]
    fn an_enabled_integration_reaches_its_adapter() {
        let (adapter, calls) = counting("beads");
        let registry = AdapterRegistry::new().with(adapter);
        let set =
            parse_integrations("[integrations.beads]\ncommand = \"bd\"\nenabled = true\n").unwrap();

        registry.build_active(&set).expect("builds");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_enabled_integration_with_no_adapter_fails_the_boot() {
        // Refusing is the safe direction: the configuration claims a capability
        // this binary cannot provide, and booting anyway would hide that.
        let registry = AdapterRegistry::new();
        let set =
            parse_integrations("[integrations.beads]\ncommand = \"bd\"\nenabled = true\n").unwrap();

        let err = match registry.build_active(&set) {
            Err(e) => e,
            Ok(_) => panic!("an unmatched enabled integration must fail"),
        };

        match err {
            RegistryError::NoAdapter { name, known } => {
                assert_eq!(name, "beads");
                assert_eq!(known, "none");
            }
            other => panic!("expected NoAdapter, got {other:?}"),
        }
    }

    #[test]
    fn a_disabled_integration_with_no_adapter_is_not_an_error() {
        // Declaring something this binary cannot serve is fine as long as it is
        // off — an operator may share one config file across builds.
        let registry = AdapterRegistry::new();
        let set = parse_integrations("[integrations.beads]\ncommand = \"bd\"\n").unwrap();
        assert!(registry.build_active(&set).expect("off is fine").is_empty());
    }

    #[test]
    fn an_adapter_refusal_propagates_rather_than_being_swallowed() {
        let (adapter, _) = counting("obsidian");
        let registry = AdapterRegistry::new().with(adapter);
        // The counting adapter rejects directory endpoints.
        let set =
            parse_integrations("[integrations.obsidian]\nroot = \"/v\"\nenabled = true\n").unwrap();

        let err = match registry.build_active(&set) {
            Err(e) => e,
            Ok(_) => panic!("adapter refuses"),
        };
        assert!(matches!(
            err,
            RegistryError::Adapter(AdapterError::WrongEndpointKind { .. })
        ));
    }

    #[test]
    fn an_empty_set_builds_nothing_and_consults_nobody() {
        let (adapter, calls) = counting("beads");
        let registry = AdapterRegistry::new().with(adapter);

        let tools = registry
            .build_active(&IntegrationSet::new())
            .expect("a fresh boot is valid");

        assert!(tools.is_empty(), "a fresh boot must enable nothing");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
