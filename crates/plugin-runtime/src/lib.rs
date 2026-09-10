//! ardur-plugin-runtime — the §8.8 claim-activation governance layer.
//!
//! Plan family: §8.8
//! (`plans/8.8-plugin-runtime-claims-tool-channel-provider-extension-blueprint.md`).
//!
//! # Scope
//!
//! The plan describes a much larger architecture (Cedar-gated per-trait
//! actions, wasm/process/container sandbox backends, a `Provider` extension
//! family bridged into a live `ProviderRegistry`, refresh-on-update claim
//! diffing). None of that substrate exists in this codebase yet, and a real
//! sandboxed executor for untrusted third-party plugin code is out-of-scope
//! engineering to build safely in one pass. This crate implements the real,
//! honestly-bounded subset: **claim declaration validation** ([`ClaimSet`])
//! and **claim activation governance** ([`PluginRuntimeHost`]) for the two
//! trait families this codebase has a genuine live registry for — `Tool`
//! (`ardur_tool_registry::ToolRegistry`) and `Channel`
//! (`ardur_messaging_gateway::GatewayRegistry`).
//!
//! Activation does **not** load or execute untrusted code. The host process
//! supplies an already-constructed `Box<dyn Tool>` / `Box<dyn
//! MessagingGateway>` (e.g. from a boot-time built-in bridge, or a future
//! sandboxed loader); this crate's job is to verify that implementation
//! against the plugin's signed, admitted manifest (does it claim only the
//! capabilities the manifest declared?), give it a namespaced identity no
//! built-in registration can collide with, and derive a cap-token strictly
//! narrower than the plugin's admission token before registering it.
//!
//! `Provider` extension loading is deliberately **not** implemented: this
//! codebase's `ProviderRegistry` (`ardur_provider_runtime`) exists as a type
//! but is not consulted live by `FusedRuntime` (the provider is chosen once
//! at boot) — bridging a plugin-supplied `Provider` into it today would
//! register something nothing ever calls. Left for a follow-up once
//! `FusedRuntime` gains live provider-registry consultation.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod claim;
mod error;
mod host;
mod namespace;

pub use claim::{
    ClaimSet, ClaimedTraitFamily, MAX_CLAIM_NAME_LEN, MAX_CLAIMS_PER_PLUGIN, RuntimeClaim,
};
pub use error::ClaimError;
pub use host::{ActivatedChannelClaim, ActivatedToolClaim, PluginRuntimeHost};
pub use namespace::{namespaced_channel_id, namespaced_tool_id};
