//! [`ClaimError`] — every way a runtime claim can be refused.

use ardur_tool_registry::Capability;

use crate::claim::ClaimedTraitFamily;

/// Every way declaring or activating a [`crate::RuntimeClaim`] can fail.
#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    /// The plugin id fails the `[a-zA-Z0-9_-]{1,64}` charset/length check.
    #[error("plugin id `{0}` must be 1-64 characters of [a-zA-Z0-9_-]")]
    InvalidPluginId(String),

    /// A claim name fails the `[a-zA-Z0-9_-]{1,64}` charset/length check.
    #[error("claim name `{0}` must be 1-64 characters of [a-zA-Z0-9_-]")]
    InvalidClaimName(String),

    /// Two claims in the same set named the same `(family, name)` pair.
    #[error("duplicate claim `{name}` in family {family:?}")]
    DuplicateClaim {
        /// The repeated claim name.
        name: String,
        /// The trait family both claims declared.
        family: ClaimedTraitFamily,
    },

    /// A plugin declared more claims than [`crate::MAX_CLAIMS_PER_PLUGIN`].
    #[error("plugin declares {0} claims, exceeding the {1} ceiling")]
    TooManyClaims(usize, usize),

    /// The claim being activated is not present in the plugin's validated
    /// [`crate::ClaimSet`].
    #[error("claim `{0}` is not declared in the plugin's claim set")]
    UnknownClaim(String),

    /// The supplied `Tool` requires a capability the plugin's admitted
    /// manifest never declared. The manifest — signed and admitted before
    /// this point — is the authority on what a plugin may touch; a supplied
    /// implementation cannot silently claim more at activation time.
    #[error(
        "tool `{tool_id}` requires undeclared capability {capability:?}; \
         the manifest only declared {declared:?}"
    )]
    UndeclaredCapability {
        /// The tool's own (pre-namespace) id, for diagnostics.
        tool_id: String,
        /// The capability the tool required but the manifest never declared.
        capability: Capability,
        /// The full declared ceiling, for diagnostics.
        declared: Vec<Capability>,
    },

    /// Deriving the claim's attenuated cap-token failed.
    #[error("cap-token attenuation failed: {0}")]
    AttenuationFailed(String),

    /// The registry (`ToolRegistry` or `GatewayRegistry`) refused the
    /// namespaced registration — almost always because this exact
    /// `(plugin_id, claim_name)` was already activated once.
    #[error("registration refused: {0}")]
    RegistrationFailed(String),
}
