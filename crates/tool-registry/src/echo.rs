//! [`EchoTool`] — a concrete, capability-free [`Tool`] that returns its input
//! verbatim. Used by the registry tests and as a sanity demo of the trait.

use async_trait::async_trait;
use serde_json::json;

use ardur_runtime::CostTuple;

use crate::capability::Capability;
use crate::error::ToolError;
use crate::tool::{Tool, ToolContext, ToolId, ToolOutput, ToolSchema};

/// A tool that echoes its arguments back as its output.
///
/// It requires no [`Capability`] — echoing touches no resource — so it is the
/// minimal end-to-end exercise of the [`Tool`] contract: a stable [`ToolId`], a
/// schema, and an [`invoke`](Tool::invoke) that round-trips its input.
pub struct EchoTool {
    schema: ToolSchema,
}

impl EchoTool {
    /// The id [`EchoTool`] registers under.
    pub const ID: &'static str = "echo";

    /// Construct an [`EchoTool`] with its fixed schema.
    #[must_use]
    pub fn new() -> Self {
        let schema = ToolSchema {
            description: "Returns its input arguments unchanged.".to_string(),
            // Echo accepts any JSON object and returns it verbatim.
            input_schema: json!({ "type": "object" }),
            output_schema: json!({ "type": "object" }),
            examples: vec![],
        };
        Self { schema }
    }
}

impl Default for EchoTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn id(&self) -> ToolId {
        ToolId::new(Self::ID)
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        _ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: args.clone(),
            cost: CostTuple::default(),
            receipt_data: args,
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &[]
    }
}
