use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

pub type ToolInput = Value;
pub type ToolOutput = Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub id: String,
    pub name: String,
    pub description: String,
    pub parameters: Vec<ToolParameter>,
    pub handler: String, // Name of the handler function
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolParameter {
    pub name: String,
    pub param_type: String,
    pub required: bool,
    pub description: String,
}

impl McpTool {
    pub fn new(name: &str, description: &str) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            name: name.to_string(),
            description: description.to_string(),
            parameters: Vec::new(),
            handler: name.to_string(),
        }
    }

    pub fn with_parameter(
        mut self,
        name: &str,
        param_type: &str,
        required: bool,
        description: &str,
    ) -> Self {
        self.parameters.push(ToolParameter {
            name: name.to_string(),
            param_type: param_type.to_string(),
            required,
            description: description.to_string(),
        });
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_creation() {
        let tool = McpTool::new("test_tool", "A test tool");
        assert_eq!(tool.name, "test_tool");
        assert_eq!(tool.description, "A test tool");
        assert!(tool.parameters.is_empty());
    }

    #[test]
    fn test_tool_with_parameters() {
        let tool = McpTool::new("search", "Search tool")
            .with_parameter("query", "string", true, "Search query")
            .with_parameter("limit", "integer", false, "Result limit");

        assert_eq!(tool.parameters.len(), 2);
        assert_eq!(tool.parameters[0].name, "query");
        assert!(tool.parameters[0].required);
        assert_eq!(tool.parameters[1].name, "limit");
        assert!(!tool.parameters[1].required);
    }
}
