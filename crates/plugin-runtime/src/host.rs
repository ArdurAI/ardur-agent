//! [`PluginRuntimeHost`] — validates a claim against its plugin's declared
//! set, checks a supplied `Tool`'s required capabilities against the
//! manifest's declared ceiling, derives a namespaced identity and (for `Tool`
//! claims) an attenuated cap-token, and registers the wrapped implementation
//! into the caller's registry.

use async_trait::async_trait;

use ardur_cap_token::{
    AttenuationRule, BiscuitCapTokenAttenuator, CapToken, CapTokenAttenuator, Caveat,
};
use ardur_messaging_gateway::{
    ChannelId, GatewayRegistry, IncomingMessage, MessageReceipt, MessagingGateway, OutgoingMessage,
};
use ardur_tool_registry::{
    Capability, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolRegistry, ToolSchema,
};

use crate::ClaimSet;
use crate::claim::ClaimedTraitFamily;
use crate::error::ClaimError;
use crate::namespace::{namespaced_channel_id, namespaced_tool_id};

/// The result of successfully activating a `Tool`-family claim.
#[derive(Debug)]
pub struct ActivatedToolClaim {
    /// The namespaced id the tool was registered under
    /// (`plugin/<plugin_id>/<claim_name>`).
    pub tool_id: ToolId,
    /// A cap-token derived from the plugin's admission cap, attenuated via
    /// [`AttenuationRule::RestrictTools`] to exactly this tool id — it cannot
    /// authorize any other tool, including the plugin's own other claims.
    pub claim_cap_token: CapToken,
}

/// The result of successfully activating a `Channel`-family claim.
#[derive(Debug)]
pub struct ActivatedChannelClaim {
    /// The namespaced id the channel was registered under
    /// (`plugin/<plugin_id>/<claim_name>`).
    pub channel_id: ChannelId,
    /// The plugin's admission cap-token, unchanged. **Not narrower** than the
    /// plugin's full admission scope — see the crate-level docs: Ardur's
    /// Biscuit attenuation primitives have no channel-scoped restriction
    /// analogous to [`AttenuationRule::RestrictTools`], and `MessagingGateway`
    /// send/receive is not gated through the same per-tool cap-token check
    /// `FusedRuntime` applies to tools. Namespacing still prevents a
    /// registration collision; cap-token narrowing for channels is a
    /// follow-up once a channel-scoped attenuation primitive exists.
    pub claim_cap_token: CapToken,
}

/// Governs activation of plugin-declared runtime claims into the real
/// `ToolRegistry`/`GatewayRegistry`. Holds no state — every method is a pure
/// function of its arguments — so activation is deterministic and trivially
/// testable.
#[derive(Debug, Default, Clone, Copy)]
pub struct PluginRuntimeHost;

impl PluginRuntimeHost {
    /// A fresh host.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Activate a single `Tool`-family claim from `claims`, registering the
    /// namespaced wrapper of `tool` into `registry`.
    ///
    /// Refuses with:
    /// - [`ClaimError`] variants from [`ClaimSet::validate_closure`] if the
    ///   claim set itself is malformed.
    /// - [`ClaimError::UnknownClaim`] if `claim_name` names no `Tool` claim in
    ///   `claims`.
    /// - [`ClaimError::UndeclaredCapability`] if `tool` requires a capability
    ///   the claim's `declared_capabilities` ceiling (from the plugin's
    ///   signed, admitted manifest) does not include — a supplied
    ///   implementation cannot claim more at activation time than the
    ///   manifest declared.
    /// - [`ClaimError::AttenuationFailed`] if deriving the child cap-token
    ///   fails.
    /// - [`ClaimError::RegistrationFailed`] if the namespaced id is already
    ///   registered (this exact claim was activated once already).
    pub fn activate_tool_claim(
        &self,
        claims: &ClaimSet,
        claim_name: &str,
        tool: Box<dyn Tool>,
        admission_cap: &CapToken,
        registry: &mut ToolRegistry,
    ) -> Result<ActivatedToolClaim, ClaimError> {
        claims.validate_closure()?;
        let claim = claims
            .claims
            .iter()
            .find(|c| c.family == ClaimedTraitFamily::Tool && c.name == claim_name)
            .ok_or_else(|| ClaimError::UnknownClaim(claim_name.to_string()))?;

        for required in tool.required_capabilities() {
            if !claim.declared_capabilities.contains(required) {
                return Err(ClaimError::UndeclaredCapability {
                    tool_id: tool.id().to_string(),
                    capability: required.clone(),
                    declared: claim.declared_capabilities.clone(),
                });
            }
        }

        let namespaced_id = namespaced_tool_id(&claims.plugin_id, claim_name);
        let claim_cap_token = BiscuitCapTokenAttenuator
            .attenuate(
                admission_cap,
                Caveat::new(AttenuationRule::RestrictTools(vec![
                    namespaced_id.as_str().to_string(),
                ])),
            )
            .map_err(|e| ClaimError::AttenuationFailed(e.to_string()))?;

        registry
            .register(Box::new(NamespacedTool {
                id: namespaced_id.clone(),
                inner: tool,
            }))
            .map_err(|e| ClaimError::RegistrationFailed(e.to_string()))?;

        Ok(ActivatedToolClaim {
            tool_id: namespaced_id,
            claim_cap_token,
        })
    }

    /// Activate a single `Channel`-family claim from `claims`, registering
    /// the namespaced wrapper of `gateway` into `registry`. See
    /// [`ActivatedChannelClaim`] for the cap-token narrowing caveat.
    pub fn activate_channel_claim(
        &self,
        claims: &ClaimSet,
        claim_name: &str,
        gateway: Box<dyn MessagingGateway>,
        admission_cap: &CapToken,
        registry: &mut GatewayRegistry,
    ) -> Result<ActivatedChannelClaim, ClaimError> {
        claims.validate_closure()?;
        claims
            .claims
            .iter()
            .find(|c| c.family == ClaimedTraitFamily::Channel && c.name == claim_name)
            .ok_or_else(|| ClaimError::UnknownClaim(claim_name.to_string()))?;

        let namespaced_id = namespaced_channel_id(&claims.plugin_id, claim_name);
        registry
            .register(Box::new(NamespacedGateway {
                id: namespaced_id.clone(),
                inner: gateway,
            }))
            .map_err(|e| ClaimError::RegistrationFailed(e.to_string()))?;

        Ok(ActivatedChannelClaim {
            channel_id: namespaced_id,
            claim_cap_token: admission_cap.clone(),
        })
    }
}

/// Wraps a plugin-supplied `Tool`, overriding only [`Tool::id`] with the
/// namespaced identity — every other call delegates straight through.
struct NamespacedTool {
    id: ToolId,
    inner: Box<dyn Tool>,
}

#[async_trait]
impl Tool for NamespacedTool {
    fn id(&self) -> ToolId {
        self.id.clone()
    }

    fn schema(&self) -> &ToolSchema {
        self.inner.schema()
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        self.inner.invoke(ctx, args).await
    }

    fn required_capabilities(&self) -> &[Capability] {
        self.inner.required_capabilities()
    }
}

/// Wraps a plugin-supplied `MessagingGateway`, overriding only
/// [`MessagingGateway::channel_id`] with the namespaced identity.
struct NamespacedGateway {
    id: ChannelId,
    inner: Box<dyn MessagingGateway>,
}

#[async_trait]
impl MessagingGateway for NamespacedGateway {
    async fn send_message(
        &self,
        msg: OutgoingMessage,
    ) -> Result<MessageReceipt, ardur_messaging_gateway::GatewayError> {
        self.inner.send_message(msg).await
    }

    async fn receive(&self) -> Result<IncomingMessage, ardur_messaging_gateway::GatewayError> {
        self.inner.receive().await
    }

    fn channel_id(&self) -> ChannelId {
        self.id.clone()
    }

    fn supports_threading(&self) -> bool {
        self.inner.supports_threading()
    }
}
