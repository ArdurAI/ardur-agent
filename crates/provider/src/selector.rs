use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SelectionStrategy {
    /// Always use the specified provider
    Fixed,
    /// Round-robin across available providers
    RoundRobin,
    /// Select based on lowest cost
    LowestCost,
    /// Select based on fastest response time
    Fastest,
    /// Select based on highest availability
    HighestAvailability,
    /// Fallback chain: try primary, then secondary, etc.
    Fallback,
}

#[derive(Debug, Clone)]
pub struct ProviderSelector {
    strategy: SelectionStrategy,
    fallback_chain: Vec<String>,
    current_index: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl ProviderSelector {
    pub fn new(strategy: SelectionStrategy) -> Self {
        Self {
            strategy,
            fallback_chain: Vec::new(),
            current_index: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn with_fallback_chain(mut self, chain: Vec<&str>) -> Self {
        self.fallback_chain = chain.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn select(&self, providers: &[crate::provider::Provider]) -> crate::error::Result<crate::provider::Provider> {
        match self.strategy {
            SelectionStrategy::Fixed => {
                if providers.is_empty() {
                    return Err(crate::error::ProviderError::NotAvailable("No providers available".to_string()));
                }
                Ok(providers[0].clone())
            }
            SelectionStrategy::RoundRobin => {
                if providers.is_empty() {
                    return Err(crate::error::ProviderError::NotAvailable("No providers available".to_string()));
                }
                let idx = self.current_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst) % providers.len();
                Ok(providers[idx].clone())
            }
            SelectionStrategy::LowestCost => {
                providers.iter()
                    .min_by(|a, b| {
                        let a_cost = a.models.iter().map(|m| m.cost_per_1k_input).fold(f64::INFINITY, f64::min);
                        let b_cost = b.models.iter().map(|m| m.cost_per_1k_input).fold(f64::INFINITY, f64::min);
                        a_cost.partial_cmp(&b_cost).unwrap()
                    })
                    .cloned()
                    .ok_or_else(|| crate::error::ProviderError::NotAvailable("No providers available".to_string()))
            }
            SelectionStrategy::HighestAvailability => {
                providers.iter()
                    .filter(|p| p.status == crate::provider::ProviderStatus::Available)
                    .next()
                    .cloned()
                    .ok_or_else(|| crate::error::ProviderError::NotAvailable("No available providers".to_string()))
            }
            SelectionStrategy::Fallback => {
                for name in &self.fallback_chain {
                    if let Some(provider) = providers.iter().find(|p| &p.name == name && p.status == crate::provider::ProviderStatus::Available) {
                        return Ok(provider.clone());
                    }
                }
                Err(crate::error::ProviderError::NotAvailable("All providers in fallback chain unavailable".to_string()))
            }
            SelectionStrategy::Fastest => {
                providers.iter()
                    .min_by(|a, b| a.usage_count.cmp(&b.usage_count))
                    .cloned()
                    .ok_or_else(|| crate::error::ProviderError::NotAvailable("No providers available".to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ModelInfo, Provider, ProviderConfig, ProviderStatus};

    fn create_test_provider(name: &str, cost: f64) -> Provider {
        Provider::new(name, "test")
            .with_config(ProviderConfig::default())
            .add_model(ModelInfo {
                id: "model-1".to_string(),
                name: "Model 1".to_string(),
                provider: name.to_string(),
                max_tokens: 4096,
                supports_streaming: true,
                supports_tools: true,
                cost_per_1k_input: cost,
                cost_per_1k_output: cost * 2.0,
            })
    }

    #[test]
    fn test_selector_fixed() {
        let selector = ProviderSelector::new(SelectionStrategy::Fixed);
        let providers = vec![
            create_test_provider("A", 0.01),
            create_test_provider("B", 0.02),
        ];
        let selected = selector.select(&providers).unwrap();
        assert_eq!(selected.name, "A");
    }

    #[test]
    fn test_selector_round_robin() {
        let selector = ProviderSelector::new(SelectionStrategy::RoundRobin);
        let providers = vec![
            create_test_provider("A", 0.01),
            create_test_provider("B", 0.02),
        ];
        let p1 = selector.select(&providers).unwrap();
        let p2 = selector.select(&providers).unwrap();
        assert_ne!(p1.name, p2.name);
    }

    #[test]
    fn test_selector_lowest_cost() {
        let selector = ProviderSelector::new(SelectionStrategy::LowestCost);
        let providers = vec![
            create_test_provider("Expensive", 0.10),
            create_test_provider("Cheap", 0.01),
        ];
        let selected = selector.select(&providers).unwrap();
        assert_eq!(selected.name, "Cheap");
    }

    #[test]
    fn test_selector_fallback() {
        let selector = ProviderSelector::new(SelectionStrategy::Fallback)
            .with_fallback_chain(vec!["Primary", "Secondary"]);
        let mut primary = create_test_provider("Primary", 0.01);
        primary.set_status(ProviderStatus::Offline);
        let secondary = create_test_provider("Secondary", 0.02);
        let providers = vec![primary, secondary];
        let selected = selector.select(&providers).unwrap();
        assert_eq!(selected.name, "Secondary");
    }

    #[test]
    fn test_selector_highest_availability() {
        let selector = ProviderSelector::new(SelectionStrategy::HighestAvailability);
        let mut p1 = create_test_provider("Busy", 0.01);
        p1.set_status(ProviderStatus::Busy);
        let p2 = create_test_provider("Available", 0.02);
        let providers = vec![p1, p2];
        let selected = selector.select(&providers).unwrap();
        assert_eq!(selected.name, "Available");
    }

    #[test]
    fn test_selector_empty_providers() {
        let selector = ProviderSelector::new(SelectionStrategy::Fixed);
        let providers: Vec<Provider> = vec![];
        assert!(selector.select(&providers).is_err());
    }
}
