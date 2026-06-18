//! [`ArdurMcpServer`] — the `rmcp` [`ServerHandler`] that exposes a shared
//! [`ToolRegistry`] over MCP.
//!
//! It implements just the tool half of the protocol: [`list_tools`] enumerates
//! the registry, and [`call_tool`] resolves the named tool and runs its
//! [`Tool::invoke`](crate::Tool::invoke). The other `ServerHandler` methods keep
//! their defaults (method-not-found / empty), so resources, prompts, and
//! sampling are simply unadvertised.
//!
//! [`list_tools`]: ServerHandler::list_tools
//! [`call_tool`]: ServerHandler::call_tool

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool as McpTool,
};
use rmcp::service::{RequestContext, RoleServer};

use ardur_runtime::{CapTokenRef, SessionId};

use crate::registry::ToolRegistry;
use crate::tool::{InvocationId, Tool, ToolContext, ToolId};

/// An `rmcp` server handler backed by a shared [`ToolRegistry`].
///
/// Cloning is cheap (the registry is shared behind an [`Arc`]); the
/// Streamable-HTTP transport clones a fresh handler per session via its service
/// factory.
#[derive(Clone)]
pub struct ArdurMcpServer {
    registry: Arc<ToolRegistry>,
}

impl ArdurMcpServer {
    /// Wrap `registry` as an MCP server handler.
    #[must_use]
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }

    /// Project an ardur [`Tool`] onto the wire `tools/list` shape: its id is the
    /// MCP tool name, its description and JSON-Schema input flow through verbatim.
    fn to_mcp_tool(tool: &dyn Tool) -> McpTool {
        let schema = tool.schema();
        // MCP's `input_schema` is a JSON *object*; a non-object schema (there is
        // none in practice) degrades to an empty object rather than panicking.
        let input = schema.input_schema.as_object().cloned().unwrap_or_default();
        McpTool::new(tool.id().0, schema.description.clone(), Arc::new(input))
    }

    /// The ambient context an MCP-driven invocation runs against.
    ///
    /// MCP-exposed tools are capability-free (echo, health-check), so the
    /// context carries no real cap-token and a wide budget. `// TODO §6.0
    /// Phase 3:` derive the context from the request's verified bearer identity
    /// once MCP calls are gated by the cap-token + Cedar layers.
    fn invocation_context() -> ToolContext {
        ToolContext {
            cap_token: CapTokenRef(String::new()),
            session_id: SessionId::new(),
            invocation_id: InvocationId::new(),
            cwd: std::env::current_dir().unwrap_or_default(),
            env: HashMap::new(),
            cost_budget_cents: u32::MAX,
        }
    }
}

impl ServerHandler for ArdurMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Ardur's local tool registry, exposed over the Model Context Protocol.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self
            .registry
            .list()
            .into_iter()
            .filter(|tool| tool.required_capabilities().is_empty())
            .map(Self::to_mcp_tool)
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let id = ToolId::new(request.name.as_ref());
        let Some(tool) = self.registry.get(&id) else {
            return Err(McpError::invalid_params(
                format!("unknown tool: {}", request.name),
                None,
            ));
        };
        if !tool.required_capabilities().is_empty() {
            return Err(McpError::invalid_params(
                format!(
                    "tool {} requires fused-runtime cap-token/Cedar context and is not callable over direct MCP",
                    request.name
                ),
                None,
            ));
        }

        // MCP arguments are an object (or absent); pass them through as the
        // tool's JSON `args`, defaulting an omitted object to `{}`.
        let args = request
            .arguments
            .map_or(serde_json::Value::Object(Default::default()), |obj| {
                serde_json::Value::Object(obj)
            });

        let ctx = Self::invocation_context();
        match tool.invoke(&ctx, args).await {
            // A successful call returns the tool's content as the structured
            // result (with a text mirror for non-structured clients).
            Ok(output) => Ok(CallToolResult::structured(output.content)),
            // A tool failure is reported as an MCP tool-error result (not a
            // protocol error) so the client sees `isError: true` with the cause.
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }
}
