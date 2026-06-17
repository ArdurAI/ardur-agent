//! LLM helper tools implementing the Tool trait.

use std::sync::Arc;
use async_trait::async_trait;
use serde_json::json;
use ardur_runtime::CostTuple;
use ardur_tool_registry::{Capability, Tool, ToolContext, ToolId, ToolOutput, ToolSchema};
use crate::accounting::TokenAccountant;
use crate::error::LlmHelperError;

/// `llm.task` — execute an LLM task with token accounting.
pub struct LlmTaskTool {
    accountant: Arc<parking_lot::RwLock<TokenAccountant>>,
}

impl LlmTaskTool {
    pub fn new() -> Self {
        Self { accountant: Arc::new(parking_lot::RwLock::new(TokenAccountant::new())) }
    }
}

#[async_trait]
impl Tool for LlmTaskTool {
    fn id(&self) -> ToolId { ToolId::new("llm.task") }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Execute an LLM task with token budget tracking.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "prompt": { "type": "string" },
                    "max_tokens": { "type": "integer", "default": 1000 },
                    "max_cost_cents": { "type": "integer", "default": 100 }
                },
                "required": ["task_id", "prompt"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "result": { "type": "string" },
                    "tokens_used": { "type": "integer" },
                    "cost_cents": { "type": "integer" }
                }
            }),
            examples: vec![],
        })
    }

    async fn invoke(&self, _ctx: &ToolContext, args: serde_json::Value) -> Result<ToolOutput, ardur_tool_registry::ToolError> {
        let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let max_tokens = args.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(1000);
        let max_cost_cents = args.get("max_cost_cents").and_then(|v| v.as_u64()).unwrap_or(100);

        let mut accountant = self.accountant.write();
        accountant.create_budget(&task_id, max_tokens, max_cost_cents);

        // Simulate LLM execution
        let tokens_in = prompt.len() as u64 / 4;
        let tokens_out = 100u64;
        let cost_cents = 5u64;

        if let Err(e) = accountant.record(&task_id, tokens_in, tokens_out, cost_cents, "mock", "mock-model") {
            return Err(ardur_tool_registry::ToolError::ExecutionFailed(e));
        }

        let budget = accountant.get_budget(&task_id).unwrap();

        Ok(ToolOutput {
            content: json!({
                "result": format!("Processed task: {}", task_id),
                "tokens_used": budget.used_tokens,
                "cost_cents": budget.used_cost_cents,
                "remaining_tokens": budget.remaining_tokens(),
                "remaining_cost_cents": budget.remaining_cost_cents(),
            }),
            cost: CostTuple::default(),
            receipt_data: json!({"action": "llm.task", "task_id": task_id, "permitted": true}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![Capability::NetworkOut, Capability::Custom("llm-helper".to_string())]
        });
        &CAPS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_task_tool_id() {
        let tool = LlmTaskTool::new();
        assert_eq!(tool.id().as_str(), "llm.task");
    }
}
