//! Search provider implementations.

use serde::{Deserialize, Serialize};
use chrono::Utc;

/// A single search result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub source: String,
    pub timestamp: String,
    pub confidence: u8,
}

/// Trait for search providers.
pub trait SearchProvider: Send + Sync {
    fn name(&self) -> &str;
    fn search(&self, query: &str) -> Result<Vec<SearchResult>, String>;
}

/// Brave Search provider.
pub struct BraveProvider {
    api_key: String,
}

impl BraveProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self { api_key: api_key.into() }
    }
}

impl SearchProvider for BraveProvider {
    fn name(&self) -> &str { "brave" }
    fn search(&self, query: &str) -> Result<Vec<SearchResult>, String> {
        Ok(vec![SearchResult {
            title: format!("Brave result for '{query}'"),
            url: "https://example.com/brave".to_string(),
            snippet: "Mock Brave search result".to_string(),
            source: "Brave".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            confidence: 90,
        }])
    }
}

/// DuckDuckGo provider.
pub struct DuckDuckGoProvider;

impl DuckDuckGoProvider {
    pub fn new() -> Self { Self }
}

impl SearchProvider for DuckDuckGoProvider {
    fn name(&self) -> &str { "duckduckgo" }
    fn search(&self, query: &str) -> Result<Vec<SearchResult>, String> {
        Ok(vec![SearchResult {
            title: format!("DDG result for '{query}'"),
            url: "https://example.com/ddg".to_string(),
            snippet: "Mock DDG search result".to_string(),
            source: "DuckDuckGo".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            confidence: 85,
        }])
    }
}

/// SearXNG provider.
pub struct SearxngProvider {
    instance_url: String,
}

impl SearxngProvider {
    pub fn new(url: impl Into<String>) -> Self {
        Self { instance_url: url.into() }
    }
}

impl SearchProvider for SearxngProvider {
    fn name(&self) -> &str { "searxng" }
    fn search(&self, query: &str) -> Result<Vec<SearchResult>, String> {
        Ok(vec![SearchResult {
            title: format!("SearXNG result for '{query}'"),
            url: format!("{}/search?q={}", self.instance_url, query),
            snippet: "Mock SearXNG result".to_string(),
            source: "SearXNG".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            confidence: 80,
        }])
    }
}

/// Tavily provider.
pub struct TavilyProvider {
    api_key: String,
}

impl TavilyProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self { api_key: api_key.into() }
    }
}

impl SearchProvider for TavilyProvider {
    fn name(&self) -> &str { "tavily" }
    fn search(&self, query: &str) -> Result<Vec<SearchResult>, String> {
        Ok(vec![SearchResult {
            title: format!("Tavily result for '{query}'"),
            url: "https://example.com/tavily".to_string(),
            snippet: "Mock Tavily search result".to_string(),
            source: "Tavily".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            confidence: 88,
        }])
    }
}

/// Firecrawl provider.
pub struct FirecrawlProvider {
    api_key: String,
}

impl FirecrawlProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self { api_key: api_key.into() }
    }
}

impl SearchProvider for FirecrawlProvider {
    fn name(&self) -> &str { "firecrawl" }
    fn search(&self, query: &str) -> Result<Vec<SearchResult>, String> {
        Ok(vec![SearchResult {
            title: format!("Firecrawl result for '{query}'"),
            url: "https://example.com/firecrawl".to_string(),
            snippet: "Mock Firecrawl search result".to_string(),
            source: "Firecrawl".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            confidence: 82,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brave_search_mock() {
        let provider = BraveProvider::new("test-key");
        let results = provider.search("rust").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, "Brave");
    }

    #[test]
    fn ddg_search_mock() {
        let provider = DuckDuckGoProvider::new();
        let results = provider.search("rust").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, "DuckDuckGo");
    }

    #[test]
    fn multi_provider_fallback() {
        let providers: Vec<Box<dyn SearchProvider>> = vec![
            Box::new(BraveProvider::new("key")),
            Box::new(DuckDuckGoProvider::new()),
        ];
        let mut all_results = vec![];
        for p in &providers {
            if let Ok(r) = p.search("test") {
                all_results.extend(r);
            }
        }
        assert_eq!(all_results.len(), 2);
    }
}
