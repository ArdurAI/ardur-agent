//! [`ProviderRegistry`] — the name→provider lookup the runtime resolves a
//! [`ProviderId`] against before dispatching a turn.

use std::collections::HashMap;
use std::sync::Arc;

use ardur_runtime::ProviderId;

use crate::provider::Provider;

/// An in-process registry of providers, keyed by [`ProviderId`].
///
/// Providers are held behind `Arc` so a resolved handle can be shared across
/// concurrent turns without re-locking the registry.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: HashMap<ProviderId, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `provider` under its own [`Provider::id`], replacing any prior
    /// entry with the same id and returning the displaced provider, if any.
    pub fn register(&mut self, provider: Arc<dyn Provider>) -> Option<Arc<dyn Provider>> {
        self.providers.insert(provider.id(), provider)
    }

    /// Resolve a provider by id, returning a shared handle.
    #[must_use]
    pub fn get(&self, id: &ProviderId) -> Option<Arc<dyn Provider>> {
        self.providers.get(id).cloned()
    }

    /// The ids of every registered provider, in unspecified order.
    #[must_use]
    pub fn list(&self) -> Vec<ProviderId> {
        self.providers.keys().cloned().collect()
    }
}
