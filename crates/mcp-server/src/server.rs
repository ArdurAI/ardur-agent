use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::{McpError, Result};
use crate::tool::{McpTool, ToolInput, ToolOutput};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub name: String,
    pub version: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: "ardur-mcp-server".to_string(),
            version: "0.1.0".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpServer {
    config: ServerConfig,
    tools: Arc<RwLock<HashMap<String, McpTool>>>,
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new(ServerConfig::default())
    }
}

impl McpServer {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            tools: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn register_tool(&self, tool: McpTool) -> Result<()> {
        let mut tools = self.tools.write().map_err(|_| {
            McpError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        tools.insert(tool.name.clone(), tool);
        Ok(())
    }

    pub fn get_tool(&self, name: &str) -> Result<McpTool> {
        let tools = self.tools.read().map_err(|_| {
            McpError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        tools.get(name).cloned().ok_or_else(|| McpError::ToolNotFound(name.to_string()))
    }

    pub fn list_tools(&self) -> Result<Vec<McpTool>> {
        let tools = self.tools.read().map_err(|_| {
            McpError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(tools.values().cloned().collect())
    }

    pub fn remove_tool(&self, name: &str) -> Result<()> {
        let mut tools = self.tools.write().map_err(|_| {
            McpError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        tools.remove(name).ok_or_else(|| McpError::ToolNotFound(name.to_string()))?;
        Ok(())
    }

    pub fn execute_tool(&self, name: &str, input: ToolInput) -> Result<ToolOutput> {
        let _tool = self.get_tool(name)?;
        // In a real implementation, this would dispatch to the handler
        // For now, return the input as output (echo)
        Ok(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_creation() {
        let server = McpServer::default();
        let tools = server.list_tools().unwrap();
        assert!(tools.is_empty());
    }

    #[test]
    fn test_register_and_get_tool() {
        let server = McpServer::default();
        let tool = McpTool::new("test", "A test tool");
        server.register_tool(tool.clone()).unwrap();
        let retrieved = server.get_tool("test").unwrap();
        assert_eq!(retrieved.name, "test");
    }

    #[test]
    fn test_list_tools() {
        let server = McpServer::default();
        server.register_tool(McpTool::new("tool1", "First")).unwrap();
        server.register_tool(McpTool::new("tool2", "Second")).unwrap();
        let tools = server.list_tools().unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn test_remove_tool() {
        let server = McpServer::default();
        server.register_tool(McpTool::new("remove_me", "To be removed")).unwrap();
        server.remove_tool("remove_me").unwrap();
        assert!(server.get_tool("remove_me").is_err());
    }

    #[test]
    fn test_execute_tool() {
        let server = McpServer::default();
        server.register_tool(McpTool::new("echo", "Echo tool")).unwrap();
        let input = serde_json::json!({"message": "hello"});
        let output = server.execute_tool("echo", input.clone()).unwrap();
        assert_eq!(output, input);
    }
}
