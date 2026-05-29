//! Shared test fixtures: a configurable [`CapTool`] that carries an arbitrary
//! id and capability set so the registry's capability queries can be exercised.

use async_trait::async_trait;
use serde_json::json;

use ardur_tool_registry::{
    Capability, CostTuple, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolSchema,
};

/// A no-op tool that advertises whatever capabilities it is built with.
pub struct CapTool {
    id: ToolId,
    caps: Vec<Capability>,
    schema: ToolSchema,
}

impl CapTool {
    /// Build a tool with the given id and required capabilities.
    pub fn new(id: &str, caps: Vec<Capability>) -> Self {
        Self {
            id: ToolId::new(id),
            caps,
            schema: ToolSchema {
                description: format!("test tool {id}"),
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                examples: vec![],
            },
        }
    }
}

#[async_trait]
impl Tool for CapTool {
    fn id(&self) -> ToolId {
        self.id.clone()
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: json!({}),
            cost: CostTuple::default(),
            receipt_data: json!({}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &self.caps
    }
}
