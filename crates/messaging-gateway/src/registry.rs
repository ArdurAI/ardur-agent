//! [`GatewayRegistry`] — channel-id → gateway resolution.

use std::collections::HashMap;

use crate::error::RegistryError;
use crate::gateway::MessagingGateway;
use crate::types::ChannelId;

/// A directory of registered gateways, keyed by [`ChannelId`].
///
/// The runtime resolves the gateway for an outgoing message's channel through
/// [`get`](Self::get), then sends through it.
#[derive(Default)]
pub struct GatewayRegistry {
    gateways: HashMap<ChannelId, Box<dyn MessagingGateway>>,
}

impl GatewayRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `gateway` under its own [`channel_id`]. Returns
    /// [`RegistryError::AlreadyRegistered`] if that channel is already taken —
    /// registration never silently replaces an existing gateway.
    ///
    /// [`channel_id`]: MessagingGateway::channel_id
    pub fn register(&mut self, gateway: Box<dyn MessagingGateway>) -> Result<(), RegistryError> {
        let id = gateway.channel_id();
        if self.gateways.contains_key(&id) {
            return Err(RegistryError::AlreadyRegistered(id));
        }
        self.gateways.insert(id, gateway);
        Ok(())
    }

    /// Look up the gateway serving `id`, if any.
    #[must_use]
    pub fn get(&self, id: &ChannelId) -> Option<&dyn MessagingGateway> {
        self.gateways.get(id).map(Box::as_ref)
    }
}
