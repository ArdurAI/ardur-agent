//! [`RemoteMcpToolset`] — the client half: connect to a remote MCP server over
//! Streamable HTTP, fetch its `tools/list`, and surface each remote tool as a
//! local [`Tool`] whose [`invoke`](Tool::invoke) forwards a `tools/call`.
//!
//! The connection is owned by an [`Arc`]-shared `rmcp` running service; every
//! [`RemoteMcpTool`] wrapper clones that handle, so all wrappers fetched from
//! one server multiplex over the single underlying session.

use std::sync::Arc;

use async_trait::async_trait;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::Value;

use ardur_runtime::CostTuple;

use crate::capability::Capability;
use crate::error::ToolError;
use crate::tool::{Tool, ToolContext, ToolId, ToolOutput, ToolSchema};

/// A live connection to one remote MCP server.
///
/// Built with [`connect`](Self::connect); its [`into_tools`](Self::into_tools)
/// drains the server's tool list into registerable [`RemoteMcpTool`] wrappers.
pub struct RemoteMcpToolset {
    service: Arc<RunningService<RoleClient, ()>>,
}

impl RemoteMcpToolset {
    /// Connect to the MCP server at `uri` (its Streamable-HTTP endpoint),
    /// optionally presenting `bearer` as the `Authorization: Bearer …` token.
    ///
    /// Completes the MCP `initialize` handshake before returning.
    ///
    /// # Errors
    /// [`ToolError::Internal`] if the transport cannot connect or the handshake
    /// fails.
    pub async fn connect(
        uri: impl Into<Arc<str>>,
        bearer: Option<String>,
    ) -> Result<Self, ToolError> {
        let mut config = StreamableHttpClientTransportConfig::with_uri(uri);
        if let Some(token) = bearer {
            config = config.auth_header(token);
        }
        let transport = StreamableHttpClientTransport::from_config(config);
        let service = ()
            .serve(transport)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("MCP connect failed: {e}")))?;
        Ok(Self {
            service: Arc::new(service),
        })
    }

    /// The names the remote server advertises in `tools/list`.
    ///
    /// # Errors
    /// [`ToolError::Internal`] if the `tools/list` request fails.
    pub async fn list_tool_names(&self) -> Result<Vec<String>, ToolError> {
        let tools = self
            .service
            .list_all_tools()
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("tools/list failed: {e}")))?;
        Ok(tools.into_iter().map(|t| t.name.into_owned()).collect())
    }

    /// Fetch the remote `tools/list` and wrap each entry as a registerable
    /// [`Tool`]. The wrappers share this connection.
    ///
    /// # Errors
    /// [`ToolError::Internal`] if the `tools/list` request fails.
    pub async fn into_tools(self) -> Result<Vec<Box<dyn Tool>>, ToolError> {
        let remote = self
            .service
            .list_all_tools()
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("tools/list failed: {e}")))?;
        let service = self.service;
        Ok(remote
            .into_iter()
            .map(|t| {
                let schema = ToolSchema {
                    description: t.description.map(|d| d.into_owned()).unwrap_or_default(),
                    input_schema: Value::Object((*t.input_schema).clone()),
                    // The remote may not declare an output schema; an open object
                    // accepts whatever the forwarded call returns.
                    output_schema: t.output_schema.map_or_else(
                        || Value::Object(Default::default()),
                        |s| Value::Object((*s).clone()),
                    ),
                    examples: vec![],
                };
                Box::new(RemoteMcpTool {
                    id: ToolId::new(t.name.into_owned()),
                    schema,
                    service: service.clone(),
                }) as Box<dyn Tool>
            })
            .collect())
    }
}

/// A single remote MCP tool, presented as a local [`Tool`].
///
/// Its [`invoke`](Tool::invoke) forwards the arguments as a `tools/call` over
/// the shared connection and returns the remote result. It declares no local
/// [`Capability`] — the remote server enforces its own authorization.
pub struct RemoteMcpTool {
    id: ToolId,
    schema: ToolSchema,
    service: Arc<RunningService<RoleClient, ()>>,
}

#[async_trait]
impl Tool for RemoteMcpTool {
    fn id(&self) -> ToolId {
        self.id.clone()
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(&self, _ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        // MCP `arguments` is an object; coerce a non-object payload to none.
        let arguments = match args {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                return Err(ToolError::InvalidArgs(format!(
                    "MCP tool arguments must be a JSON object, got {other}"
                )));
            }
        };

        let mut params = CallToolRequestParams::new(self.id.0.clone());
        params.arguments = arguments;
        let result = self
            .service
            .call_tool(params)
            .await
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("tools/call failed: {e}")))?;

        // A remote error surfaces as a failed invocation, carrying the server's
        // text content as the cause.
        if result.is_error.unwrap_or(false) {
            let message = result
                .content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ToolError::ExecutionFailed(message));
        }

        // Prefer the structured result; fall back to concatenated text content.
        let content = result.structured_content.unwrap_or_else(|| {
            let text = result
                .content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("");
            Value::String(text)
        });

        Ok(ToolOutput {
            content: content.clone(),
            cost: CostTuple::default(),
            receipt_data: content,
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &[]
    }
}
