//! PDF tools implementing the Tool trait.

use std::sync::Arc;
use async_trait::async_trait;
use serde_json::json;
use ardur_runtime::CostTuple;
use ardur_tool_registry::{Capability, Tool, ToolContext, ToolId, ToolOutput, ToolSchema};
use crate::extractor::PdfExtractor;

/// `pdf.extract` — extract text, tables, and metadata from a PDF.
pub struct PdfExtractTool;

impl PdfExtractTool {
    pub fn new() -> Self { Self }
}

#[async_trait]
impl Tool for PdfExtractTool {
    fn id(&self) -> ToolId { ToolId::new("pdf.extract") }

    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ToolSchema {
            description: "Extract text, tables, and metadata from a PDF document.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the PDF file" },
                    "format": { "type": "string", "enum": ["text", "tables", "metadata", "all"], "default": "all" }
                },
                "required": ["path"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "tables": { "type": "array" },
                    "metadata": { "type": "object" }
                }
            }),
            examples: vec![],
        })
    }

    async fn invoke(&self, _ctx: &ToolContext, args: serde_json::Value) -> Result<ToolOutput, ardur_tool_registry::ToolError> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let format = args.get("format").and_then(|v| v.as_str()).unwrap_or("all");

        let extractor = PdfExtractor::new();
        let data = std::fs::read(path)
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(format!("read error: {e}")))?;

        let doc = extractor.parse(&data)
            .map_err(|e| ardur_tool_registry::ToolError::ExecutionFailed(e))?;

        let content = match format {
            "text" => json!({"text": doc.pages.iter().map(|p| p.text.clone()).collect::<Vec<_>>().join("
")}),
            "tables" => json!({"tables": doc.pages.iter().flat_map(|p| p.tables.clone()).collect::<Vec<_>>()}),
            "metadata" => json!({"metadata": doc.metadata}),
            _ => json!({
                "text": doc.pages.iter().map(|p| p.text.clone()).collect::<Vec<_>>().join("
"),
                "tables": doc.pages.iter().flat_map(|p| p.tables.clone()).collect::<Vec<_>>(),
                "metadata": doc.metadata,
            }),
        };

        Ok(ToolOutput {
            content,
            cost: CostTuple::default(),
            receipt_data: json!({"action": "pdf.extract", "path": path, "permitted": true}),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        static CAPS: std::sync::LazyLock<Vec<Capability>> = std::sync::LazyLock::new(|| {
            vec![Capability::FsRead, Capability::Custom("pdf".to_string())]
        });
        &CAPS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_extract_tool_id() {
        let tool = PdfExtractTool::new();
        assert_eq!(tool.id().as_str(), "pdf.extract");
    }
}
