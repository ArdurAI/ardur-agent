//! Computer-use tools implementing the Tool trait.

use std::sync::Arc;
use async_trait::async_trait;
use serde_json::json;
use ardur_runtime::CostTuple;
use ardur_tool_registry::{Capability, Tool, ToolContext, ToolId, ToolOutput, ToolSchema};
use crate::macos::{MacOsAutomation, UiAction};

/// `computer.use` — perform UI automation actions on macOS.
pub struct ComputerUseTool {
    automation: Arc<MacOsAutomation>,
}

impl ComputerUseTool {
    pub fn new() -> Self {
        Self { automation: Arc::new(MacOsAutomation::new()) }
    }
}

#[async_trait]
impl Tool for ComputerUseTool {
    fn id(&self) -> ToolId { ToolId::new("computer.use") }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Perform UI automation actions on macOS (click, type, scroll, focus).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["click", "type", "press_key", "scroll", "focus", "list_elements"] },
                    "app": { "type": "string" },
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "text": { "type": "string" },
                    "key": { "type": "string" },
                    "delta": { "type": "integer" },
                    "element_id": { "type": "string" }
                },
                "required": ["action"]
            }),
            output_schema: json!({"type": "object", "properties": {"success": {"type": "boolean"}}}),
            examples: vec![],
        })
    }

    async fn invoke(&self, _ctx: &ToolContext, args: serde_json::Value) -> Result<ToolOutput, ardur_tool_registry::ToolError> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");

        let result = match action {
            "click" => {
                let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                self.automation.perform_action(&UiAction::Click { x, y })
            }
            "type" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                self.automation.perform_action(&UiAction::Type { text })
            }
            "press_key" => {
                let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("").to_string();
                self.automation.perform_action(&UiAction::PressKey { key })
            }
            "scroll" => {
                let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let delta = args.get("delta").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                self.automation.perform_action(&UiAction::Scroll { x, y, delta })
            }
            "focus" => {
                let element_id = args.get("element_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                self.automation.perform_action(&UiAction::Focus { element_id })
            }
            "list_elements" => {
                let app = args.get("app").and_then(|v| v.as_str()).unwrap_or("");
                let elements = self.automation.list_elements(app)
                    .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e))?;
                return Ok(ToolOutput {
                    content: json!({"elements": elements}),
                    cost: CostTuple::default(),
                    receipt_data: json!({"action": "computer.use", "permitted": true}),
                });
            }
            _ => return Err(ardur_tool_registry::ToolError::ExecutionFailed(format!("unknown action: {action}"))),
        };

        let success = result.map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e))?;

        Ok(ToolOutput {
            content: json!({"success": success}),
            cost: CostTuple::default(),
            receipt_data: json!({"action": "computer.use", "permitted": true}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![Capability::Custom("computer-use".to_string())]
        });
        &CAPS
    }
}

/// `computer.screenshot` — capture a screenshot of the macOS screen.
pub struct ScreenshotTool {
    automation: Arc<MacOsAutomation>,
}

impl ScreenshotTool {
    pub fn new() -> Self {
        Self { automation: Arc::new(MacOsAutomation::new()) }
    }
}

#[async_trait]
impl Tool for ScreenshotTool {
    fn id(&self) -> ToolId { ToolId::new("computer.screenshot") }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Capture a screenshot of the macOS screen.".to_string(),
            input_schema: json!({}),
            output_schema: json!({"type": "object", "properties": {"data": {"type": "string", "description": "Base64-encoded PNG"}}}),
            examples: vec![],
        })
    }

    async fn invoke(&self, _ctx: &ToolContext, _args: serde_json::Value) -> Result<ToolOutput, ardur_tool_registry::ToolError> {
        let data = self.automation.screenshot()
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e))?;
        let b64 = base64::encode(&data);
        Ok(ToolOutput {
            content: json!({"data": b64}),
            cost: CostTuple::default(),
            receipt_data: json!({"action": "computer.screenshot", "permitted": true}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![Capability::Custom("computer-use".to_string())]
        });
        &CAPS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computer_use_tool_id() {
        let tool = ComputerUseTool::new();
        assert_eq!(tool.id().as_str(), "computer.use");
    }

    #[test]
    fn screenshot_tool_id() {
        let tool = ScreenshotTool::new();
        assert_eq!(tool.id().as_str(), "computer.screenshot");
    }
}
