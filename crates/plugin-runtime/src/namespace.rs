//! `plugin/<plugin_id>/<name>` identity namespacing.
//!
//! Built-in tools and channels never carry a `plugin/` prefix, so a
//! namespaced id can never collide with one; two different plugins claiming
//! the same `<name>` land at different ids (`plugin/foo/translate` vs
//! `plugin/bar/translate`), so cross-plugin collisions are structurally
//! impossible too. The registries' own duplicate-registration rejection
//! (`RegistryError::DuplicateId` / `RegistryError::AlreadyRegistered`) is the
//! final backstop against a plugin re-registering its own claim twice.

use ardur_messaging_gateway::ChannelId;
use ardur_tool_registry::ToolId;

/// The namespaced [`ToolId`] a `Tool`-family claim registers under.
#[must_use]
pub fn namespaced_tool_id(plugin_id: &str, claim_name: &str) -> ToolId {
    ToolId::new(format!("plugin/{plugin_id}/{claim_name}"))
}

/// The namespaced [`ChannelId`] a `Channel`-family claim registers under.
#[must_use]
pub fn namespaced_channel_id(plugin_id: &str, claim_name: &str) -> ChannelId {
    ChannelId(format!("plugin/{plugin_id}/{claim_name}"))
}
