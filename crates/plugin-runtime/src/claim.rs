//! [`RuntimeClaim`] — a plugin manifest's declared intent to extend one
//! trait family — and [`ClaimSet`], the closure-invariant check over a
//! plugin's full declared set.

use ardur_tool_registry::Capability;

use crate::error::ClaimError;

/// Maximum characters in a plugin id or claim name.
pub const MAX_CLAIM_NAME_LEN: usize = 64;
/// Maximum runtime claims a single plugin manifest may declare.
pub const MAX_CLAIMS_PER_PLUGIN: usize = 16;

/// The trait family a [`RuntimeClaim`] extends. `Provider` is deliberately
/// absent — see the crate-level docs for why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClaimedTraitFamily {
    /// Extends `ardur_tool_registry::ToolRegistry`.
    Tool,
    /// Extends `ardur_messaging_gateway::GatewayRegistry`.
    Channel,
}

/// A single declared extension point. The name is pre-namespace — activation
/// derives the registry-facing identity as `plugin/<plugin_id>/<name>`.
///
/// `declared_capabilities` is meaningful only for [`ClaimedTraitFamily::Tool`]
/// claims: it is the ceiling the activated `Tool`'s
/// [`required_capabilities`](ardur_tool_registry::Tool::required_capabilities)
/// must not exceed — the manifest that was signed and admitted is the
/// authority on what a plugin may touch, not whatever the supplied `Tool`
/// impl claims about itself at activation time.
#[derive(Clone, Debug)]
pub struct RuntimeClaim {
    /// Pre-namespace extension name, e.g. `"translate"`.
    pub name: String,
    /// Which trait family this claim extends.
    pub family: ClaimedTraitFamily,
    /// The capability ceiling from the plugin's admitted manifest (`Tool`
    /// claims only; ignored for `Channel`).
    pub declared_capabilities: Vec<Capability>,
}

impl RuntimeClaim {
    /// A `Tool`-family claim declaring `declared_capabilities` as its ceiling.
    #[must_use]
    pub fn tool(name: impl Into<String>, declared_capabilities: Vec<Capability>) -> Self {
        Self {
            name: name.into(),
            family: ClaimedTraitFamily::Tool,
            declared_capabilities,
        }
    }

    /// A `Channel`-family claim.
    #[must_use]
    pub fn channel(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            family: ClaimedTraitFamily::Channel,
            declared_capabilities: Vec::new(),
        }
    }
}

fn valid_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_CLAIM_NAME_LEN
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A plugin's full declared set of runtime claims, ready for
/// [`validate_closure`](Self::validate_closure) before any activation.
pub struct ClaimSet {
    /// The plugin these claims belong to.
    pub plugin_id: String,
    /// The declared claims.
    pub claims: Vec<RuntimeClaim>,
}

impl ClaimSet {
    /// Construct a claim set without validating it — call
    /// [`validate_closure`](Self::validate_closure) before activating any
    /// claim.
    #[must_use]
    pub fn new(plugin_id: impl Into<String>, claims: Vec<RuntimeClaim>) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            claims,
        }
    }

    /// Closure invariant: a bounded count of claims, a valid plugin id and
    /// claim-name charset/length, and no duplicate `(family, name)` pair.
    /// [`PluginRuntimeHost`](crate::PluginRuntimeHost) refuses to activate
    /// any claim from a set that fails this check.
    pub fn validate_closure(&self) -> Result<(), ClaimError> {
        if !valid_identifier(&self.plugin_id) {
            return Err(ClaimError::InvalidPluginId(self.plugin_id.clone()));
        }
        if self.claims.len() > MAX_CLAIMS_PER_PLUGIN {
            return Err(ClaimError::TooManyClaims(
                self.claims.len(),
                MAX_CLAIMS_PER_PLUGIN,
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for claim in &self.claims {
            if !valid_identifier(&claim.name) {
                return Err(ClaimError::InvalidClaimName(claim.name.clone()));
            }
            if !seen.insert((claim.family, claim.name.clone())) {
                return Err(ClaimError::DuplicateClaim {
                    name: claim.name.clone(),
                    family: claim.family,
                });
            }
        }
        Ok(())
    }
}
