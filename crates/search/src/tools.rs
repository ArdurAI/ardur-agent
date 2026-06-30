//! Search tools implementing the Tool trait.

use std::sync::Arc;
use async_trait::async_trait;
use serde_json::json;
use ardur_runtime::CostTuple;
use ardur_tool_registry::{Capability, Tool, ToolContext, ToolId, ToolOutput, ToolSchema};
use crate::error::SearchError;
use crate::policy::SearchPolicy;
use crate::providers::{SearchProvider, SearchResult};

/// `web.search` — search the web using configured providers.
pub struct WebSearchTool {
    providers: Vec<Arc<dyn SearchProvider>>,
    policy: SearchPolicy,
}

impl WebSearchTool {
    pub fn new(providers: Vec<Arc<dyn SearchProvider>>, policy: SearchPolicy) -> Self {
        Self { providers, policy }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn id(&self) -> ToolId { ToolId::new("web.search") }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Search the web using multiple providers.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "provider": { "type": "string", "description": "Preferred provider (optional)" }
                },
                "required": ["query"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "results": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string" },
                                "url": { "type": "string" },
                                "snippet": { "type": "string" },
                                "source": { "type": "string" },
                                "timestamp": { "type": "string" },
                                "confidence": { "type": "integer" }
                            }
                        }
                    }
                }
            }),
            examples: vec![],
        })
    }

    async fn invoke(&self, _ctx: &ToolContext, args: serde_json::Value) -> Result<ToolOutput, ardur_tool_registry::ToolError> {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let preferred = args.get("provider").and_then(|v| v.as_str());

        let mut all_results = vec![];
        let mut last_error = None;

        for provider in &self.providers {
            if let Some(pref) = preferred {
                if provider.name() != pref {
                    continue;
                }
            }
            match provider.search(query) {
                Ok(mut results) => {
                    // Apply domain policy
                    results.retain(|r| {
                        if let Ok(url) = url::Url::parse(&r.url) {
                            if let Some(host) = url.host_str() {
                                return self.policy.check_domain(host).is_ok();
                            }
                        }
                        true
                    });
                    all_results.extend(results);
                }
                Err(e) => last_error = Some(e),
            }
        }

        if all_results.is_empty() {
            return Err(ardur_tool_registry::ToolError::ExecutionFailed(
                last_error.unwrap_or_else(|| "all providers failed".to_string())
            ));
        }

        let results_json: Vec<serde_json::Value> = all_results.iter().map(|r| {
            json!({
                "title": r.title,
                "url": r.url,
                "snippet": r.snippet,
                "source": r.source,
                "timestamp": r.timestamp,
                "confidence": r.confidence,
            })
        }).collect();

        Ok(ToolOutput {
            content: json!({ "results": results_json }),
            cost: CostTuple::default(),
            receipt_data: json!({"action": "web.search", "query": query, "permitted": true}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![Capability::NetworkOut, Capability::Custom("search".to_string())]
        });
        &CAPS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::DuckDuckGoProvider;

    #[test]
    fn web_search_tool_id() {
        let tool = WebSearchTool::new(vec![Arc::new(DuckDuckGoProvider::new())], SearchPolicy::permissive());
        assert_eq!(tool.id().as_str(), "web.search");
    }
}
