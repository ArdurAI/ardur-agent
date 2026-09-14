//! Integration configuration — the `[integrations.<name>]` surface.
//!
//! An *integration* is an external tool the runtime can be taught to drive:
//! the `bd` CLI, a Dolt clone, an Obsidian vault. This crate owns how one is
//! **declared** and **validated**; it deliberately owns nothing about how one
//! is *driven*, so that adding an adapter cannot change the meaning of the
//! configuration surface.
//!
//! # Posture
//!
//! Off by default, and off means *absent*. A fresh boot with no configuration
//! has zero integrations enabled, and an integration that is present but not
//! enabled yields no tools at all — it is not registered in a permissive state.
//! This mirrors the approval gate (ARD-463), where an empty capability list
//! builds no store rather than an empty one.
//!
//! # Why parse, rather than accept
//!
//! Every field is validated at load. A misconfigured integration is a *load*
//! error, not a surprise at first use: an operator who fat-fingers a vault path
//! should learn about it from `ardur doctor`, not from a tool call that
//! silently read the wrong directory. The parse therefore rejects anything it
//! cannot give a precise meaning to, including unknown keys — a typo'd
//! `enable = true` must not be silently ignored while the operator believes
//! the integration is on.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

mod env;
mod parse;
mod registry;

pub use env::{EnvOverrideError, apply_env_overrides};
pub use parse::{ParseError, parse_integrations};
pub use registry::{AdapterError, AdapterRegistry, IntegrationAdapter, RegistryError};

/// The name an integration is declared under: the `<name>` in
/// `[integrations.<name>]`.
///
/// Constrained to lowercase ASCII alphanumerics, `-` and `_`, because the name
/// appears in an environment-variable override (`ARDUR_INTEGRATIONS_<NAME>_*`)
/// and in a tool id (`<name>.<verb>`). Permitting a dot would make the tool id
/// ambiguous; permitting arbitrary case would make the environment mapping
/// lossy, since `FOO` cannot tell you whether it meant `foo` or `Foo`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IntegrationName(String);

impl IntegrationName {
    /// Validate and wrap a name.
    ///
    /// # Errors
    ///
    /// Returns [`NameError`] if the name is empty, over 64 bytes, or contains
    /// anything outside `[a-z0-9_-]`.
    pub fn new(raw: impl Into<String>) -> Result<Self, NameError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(NameError::Empty);
        }
        // 64 is not a technical limit; it is a "this is a name, not a payload"
        // limit, keeping derived env-var names and tool ids readable.
        if raw.len() > 64 {
            return Err(NameError::TooLong { len: raw.len() });
        }
        if let Some(bad) = raw
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_'))
        {
            return Err(NameError::IllegalCharacter { found: bad });
        }
        Ok(Self(raw))
    }

    /// The name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The environment-variable infix for this name: `beads` -> `BEADS`.
    #[must_use]
    pub fn env_infix(&self) -> String {
        self.0.to_ascii_uppercase().replace('-', "_")
    }
}

impl fmt::Display for IntegrationName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why an [`IntegrationName`] was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// `[integrations.""]`, or an empty env infix.
    #[error("an integration name cannot be empty")]
    Empty,
    /// Longer than 64 bytes.
    #[error("an integration name cannot exceed 64 bytes (got {len})")]
    TooLong {
        /// The offending length.
        len: usize,
    },
    /// Contained something outside `[a-z0-9_-]`.
    #[error(
        "an integration name may contain only lowercase letters, digits, `-` and `_` \
         (found `{found}`)"
    )]
    IllegalCharacter {
        /// The first offending character.
        found: char,
    },
}

/// Where an integration's backing resource lives.
///
/// Kept as a closed enum rather than a free-form string map so that adding a
/// new *kind* of integration is a deliberate change with a compiler-checked
/// blast radius, rather than a new magic key that silently does nothing on an
/// older binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrationEndpoint {
    /// An executable driven through argv-exec confinement (no shell string).
    ///
    /// The path is the binary to run. It is *not* resolved here: resolution is
    /// a runtime concern and a missing binary is a doctor finding, not a
    /// configuration error — an operator may legitimately write configuration
    /// on a machine that does not yet have the tool installed.
    Command {
        /// The binary to execute.
        binary: PathBuf,
    },
    /// A directory tree that confines all reads and writes.
    Directory {
        /// The root. Everything the adapter touches must resolve inside it.
        root: PathBuf,
    },
}

impl IntegrationEndpoint {
    /// A redacted, operator-facing description.
    ///
    /// Paths are shown because they are configuration, not credentials, and an
    /// operator debugging a confinement failure needs to see the root they
    /// actually configured. Nothing else about an integration is ever rendered
    /// by this crate.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Command { binary } => format!("command: {}", binary.display()),
            Self::Directory { root } => format!("directory: {}", root.display()),
        }
    }
}

/// One declared integration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Integration {
    /// The name it was declared under.
    pub name: IntegrationName,
    /// Whether it is enabled. Declaring an integration does **not** enable it:
    /// an operator can leave a configured-but-off block in place, which is the
    /// whole point of having an explicit flag rather than inferring enablement
    /// from presence.
    pub enabled: bool,
    /// Where its backing resource lives.
    pub endpoint: IntegrationEndpoint,
    /// Capability labels a caller must hold for this integration's tools.
    ///
    /// Empty means "no capability beyond whatever the tool itself declares".
    /// It does not mean unrestricted: the tool's own
    /// `required_capabilities()` still applies, and Cedar still adjudicates.
    pub capabilities: Vec<String>,
}

impl Integration {
    /// Whether this integration should contribute tools to the registry.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled
    }
}

/// Every declared integration, keyed by name.
///
/// A [`BTreeMap`] rather than a `HashMap` so that `ardur config` and doctor
/// output are ordered deterministically — an operator diffing two hosts should
/// not see spurious reordering.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IntegrationSet {
    entries: BTreeMap<IntegrationName, Integration>,
}

impl IntegrationSet {
    /// An empty set — the default posture, and what a fresh boot gets.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace an integration.
    pub fn insert(&mut self, integration: Integration) {
        self.entries.insert(integration.name.clone(), integration);
    }

    /// Look one up by name.
    #[must_use]
    pub fn get(&self, name: &IntegrationName) -> Option<&Integration> {
        self.entries.get(name)
    }

    /// Mutable access, used by the env-override pass.
    pub(crate) fn get_mut(&mut self, name: &IntegrationName) -> Option<&mut Integration> {
        self.entries.get_mut(name)
    }

    /// Every declared integration, enabled or not, in name order.
    pub fn iter(&self) -> impl Iterator<Item = &Integration> {
        self.entries.values()
    }

    /// Only the enabled ones — what the registry actually wires up.
    pub fn active(&self) -> impl Iterator<Item = &Integration> {
        self.entries.values().filter(|i| i.is_active())
    }

    /// How many are declared.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether none are declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_accepts_the_documented_alphabet_and_rejects_the_rest() {
        for good in ["beads", "dolthub", "obsidian", "my-tool", "my_tool", "x9"] {
            assert!(
                IntegrationName::new(good).is_ok(),
                "`{good}` is within the documented alphabet and must be accepted"
            );
        }

        // Uppercase is refused because the env-override mapping upcases the
        // name; accepting `Foo` would make `ARDUR_INTEGRATIONS_FOO_ENABLED`
        // ambiguous between `foo` and `Foo`.
        assert_eq!(
            IntegrationName::new("Beads"),
            Err(NameError::IllegalCharacter { found: 'B' })
        );
        // A dot would make the derived tool id `beads.v2.list` unparseable.
        assert_eq!(
            IntegrationName::new("beads.v2"),
            Err(NameError::IllegalCharacter { found: '.' })
        );
        assert_eq!(IntegrationName::new(""), Err(NameError::Empty));
    }

    #[test]
    fn the_env_infix_upcases_and_normalises_hyphens() {
        // `-` is legal in a name but not in an env var, so it maps to `_`.
        // Without this, `my-tool` would produce `ARDUR_INTEGRATIONS_MY-TOOL_*`,
        // which no shell can export.
        let name = IntegrationName::new("my-tool").expect("legal name");
        assert_eq!(name.env_infix(), "MY_TOOL");
    }

    #[test]
    fn a_fresh_set_is_empty_and_yields_nothing_active() {
        let set = IntegrationSet::new();
        assert!(set.is_empty(), "a fresh boot must declare no integrations");
        assert_eq!(set.active().count(), 0);
    }

    #[test]
    fn a_declared_but_disabled_integration_is_visible_yet_not_active() {
        // The distinction that makes an explicit `enabled` flag worth having:
        // an operator can keep a configured block in place while it is off,
        // and `ardur config` still shows it.
        let mut set = IntegrationSet::new();
        set.insert(Integration {
            name: IntegrationName::new("beads").expect("legal name"),
            enabled: false,
            endpoint: IntegrationEndpoint::Command {
                binary: PathBuf::from("bd"),
            },
            capabilities: vec![],
        });

        assert_eq!(set.len(), 1, "it is declared, so it must be listed");
        assert_eq!(
            set.active().count(),
            0,
            "but it is disabled, so it must contribute nothing"
        );
    }
}
