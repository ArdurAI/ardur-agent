use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::{ProviderError, Result};
use crate::provider::{Provider, ProviderId, ProviderStatus};

#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    providers: Arc<RwLock<HashMap<ProviderId, Provider>>>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn register(&self, provider: Provider) -> Result<ProviderId> {
        let mut providers = self.providers.write().map_err(|_| {
            ProviderError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let id = provider.id.clone();
        providers.insert(id.clone(), provider);
        Ok(id)
    }

    pub fn get(&self, id: &ProviderId) -> Result<Provider> {
        let providers = self.providers.read().map_err(|_| {
            ProviderError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        providers
            .get(id)
            .cloned()
            .ok_or_else(|| ProviderError::NotFound(id.clone()))
    }

    pub fn get_by_name(&self, name: &str) -> Result<Provider> {
        let providers = self.providers.read().map_err(|_| {
            ProviderError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        providers
            .values()
            .find(|p| p.name == name)
            .cloned()
            .ok_or_else(|| ProviderError::NotFound(name.to_string()))
    }

    pub fn list(&self) -> Result<Vec<Provider>> {
        let providers = self.providers.read().map_err(|_| {
            ProviderError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(providers.values().cloned().collect())
    }

    pub fn list_by_status(&self, status: ProviderStatus) -> Result<Vec<Provider>> {
        let providers = self.providers.read().map_err(|_| {
            ProviderError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(providers
            .values()
            .filter(|p| p.status == status)
            .cloned()
            .collect())
    }

    pub fn update(&self, provider: Provider) -> Result<()> {
        let mut providers = self.providers.write().map_err(|_| {
            ProviderError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        if !providers.contains_key(&provider.id) {
            return Err(ProviderError::NotFound(provider.id.clone()));
        }
        providers.insert(provider.id.clone(), provider);
        Ok(())
    }

    pub fn remove(&self, id: &ProviderId) -> Result<()> {
        let mut providers = self.providers.write().map_err(|_| {
            ProviderError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        providers
            .remove(id)
            .ok_or_else(|| ProviderError::NotFound(id.clone()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_register_and_get() {
        let registry = ProviderRegistry::new();
        let provider = Provider::new("OpenAI", "openai");
        let id = registry.register(provider.clone()).unwrap();
        let retrieved = registry.get(&id).unwrap();
        assert_eq!(retrieved.name, "OpenAI");
    }

    #[test]
    fn test_registry_get_by_name() {
        let registry = ProviderRegistry::new();
        let provider = Provider::new("Anthropic", "anthropic");
        registry.register(provider).unwrap();
        let retrieved = registry.get_by_name("Anthropic").unwrap();
        assert_eq!(retrieved.provider_type, "anthropic");
    }

    #[test]
    fn test_registry_list_by_status() {
        let registry = ProviderRegistry::new();
        let mut p1 = Provider::new("OpenAI", "openai");
        p1.set_status(ProviderStatus::Available);
        let mut p2 = Provider::new("Ollama", "ollama");
        p2.set_status(ProviderStatus::Offline);
        registry.register(p1).unwrap();
        registry.register(p2).unwrap();

        let available = registry.list_by_status(ProviderStatus::Available).unwrap();
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].name, "OpenAI");
    }

    #[test]
    fn test_registry_remove() {
        let registry = ProviderRegistry::new();
        let provider = Provider::new("Temp", "temp");
        let id = registry.register(provider).unwrap();
        registry.remove(&id).unwrap();
        assert!(registry.get(&id).is_err());
    }
}
