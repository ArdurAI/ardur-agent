//! [`RemoteMcpToolset`] — the client half: connect to a remote MCP server over
//! Streamable HTTP, fetch its `tools/list`, and surface each remote tool as a
//! local [`Tool`] whose [`invoke`](Tool::invoke) forwards a `tools/call`.
//!
//! The connection is owned by an [`Arc`]-shared `rmcp` running service; every
//! [`RemoteMcpTool`] wrapper clones that handle, so all wrappers fetched from
//! one server multiplex over the single underlying session.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::Value;

use ardur_resilience::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitError};
use ardur_resilience::retry::{RetryPolicy, retry_with_backoff};
use ardur_resilience::timeout::with_timeout;
use ardur_runtime::CostTuple;

use crate::capability::Capability;
use crate::error::ToolError;
use crate::tool::{Tool, ToolContext, ToolId, ToolOutput, ToolSchema};

/// Tunables for the resilience layer wrapping every network call this MCP
/// client makes: the connect handshake, `tools/list`, and `tools/call`.
///
/// `tools/call` is deliberately **never retried** — a remote tool may have
/// side effects (send a message, write a file, charge a card), and MCP gives
/// no idempotency guarantee, so resending a call that failed transport-side
/// could duplicate that side effect. `connect`/`tools/list` have no side
/// effects, so they retry freely. All three are timeout-bounded and share one
/// circuit breaker per connection, so a run of transport failures on any of
/// them fails the rest fast instead of piling up further timeouts.
#[derive(Clone, Debug)]
pub struct McpResilienceConfig {
    /// Bounds the `initialize` handshake.
    pub connect_timeout: Duration,
    /// Bounds a `tools/list` request.
    pub list_timeout: Duration,
    /// Bounds a single `tools/call` request. Not retried (see above), so this
    /// is the full budget a tool invocation gets.
    pub call_timeout: Duration,
    /// Applied to `connect` and `tools/list` only.
    pub retry_policy: RetryPolicy,
    /// Shared by `connect`, `tools/list`, and `tools/call` on one connection.
    pub circuit_breaker: CircuitBreakerConfig,
}

impl Default for McpResilienceConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            list_timeout: Duration::from_secs(30),
            call_timeout: Duration::from_secs(60),
            retry_policy: RetryPolicy::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
        }
    }
}

/// The resilience state shared by a [`RemoteMcpToolset`] and every
/// [`RemoteMcpTool`] it produces — one circuit breaker per underlying MCP
/// connection, so a failing server trips fast for every tool sourced from it.
struct McpResilience {
    list_timeout: Duration,
    call_timeout: Duration,
    retry_policy: RetryPolicy,
    breaker: CircuitBreaker,
}

/// Maps the breaker's own `Open` state onto [`ToolError`], and unwraps a
/// passed-through inner failure.
fn circuit_error_to_tool_error(err: CircuitError<ToolError>, verb: &str) -> ToolError {
    match err {
        CircuitError::Open => ToolError::Internal(anyhow::anyhow!(
            "MCP {verb}: circuit breaker open — too many recent transport failures"
        )),
        CircuitError::Inner(err) => err,
    }
}

/// The `cap.*` label every remote-MCP-sourced tool requires, so the fused
/// runtime's cap-token/Cedar gate (`authorize_tool_capabilities`) subjects it
/// to the issuing cap-token's scope instead of short-circuiting on empty caps
/// (ARD-478). The server's per-turn mint propagates this into issued tokens
/// automatically via `tool_allowlist_for_runtime`.
pub const MCP_CAPABILITY: &str = "cap.mcp";

static MCP_CAPS: std::sync::LazyLock<Vec<Capability>> =
    std::sync::LazyLock::new(|| vec![Capability::Custom("mcp".to_string())]);

/// A live connection to one remote MCP server.
///
/// Built with [`connect`](Self::connect); its [`into_tools`](Self::into_tools)
/// drains the server's tool list into registerable [`RemoteMcpTool`] wrappers.
pub struct RemoteMcpToolset {
    service: Arc<RunningService<RoleClient, ()>>,
    resilience: Arc<McpResilience>,
}

impl RemoteMcpToolset {
    /// Connect to the MCP server at `uri` (its Streamable-HTTP endpoint),
    /// optionally presenting `bearer` as the `Authorization: Bearer …` token,
    /// using the default [`McpResilienceConfig`].
    ///
    /// Completes the MCP `initialize` handshake before returning.
    ///
    /// # Errors
    /// [`ToolError::Internal`] if the transport cannot connect or the handshake
    /// fails (including after the retry budget is exhausted, or when the
    /// circuit breaker is open).
    pub async fn connect(
        uri: impl Into<Arc<str>>,
        bearer: Option<String>,
    ) -> Result<Self, ToolError> {
        Self::connect_with(uri, bearer, McpResilienceConfig::default()).await
    }

    /// [`Self::connect`] with an explicit [`McpResilienceConfig`].
    ///
    /// The handshake is timeout-bounded and retried (it has no side effects,
    /// so retrying is safe) through a circuit breaker that then also guards
    /// every `tools/list` and `tools/call` made through the returned toolset
    /// and the [`RemoteMcpTool`]s it produces.
    ///
    /// # Errors
    /// [`ToolError::Internal`] if the transport cannot connect or the handshake
    /// fails (including after the retry budget is exhausted, or when the
    /// circuit breaker is open).
    pub async fn connect_with(
        uri: impl Into<Arc<str>>,
        bearer: Option<String>,
        resilience_config: McpResilienceConfig,
    ) -> Result<Self, ToolError> {
        let uri: Arc<str> = uri.into();
        let breaker = CircuitBreaker::new(resilience_config.circuit_breaker.clone());
        let connect_timeout = resilience_config.connect_timeout;

        let service = breaker
            .call(|| {
                retry_with_backoff(
                    &resilience_config.retry_policy,
                    |_: &ToolError| true,
                    || connect_once(uri.clone(), bearer.clone(), connect_timeout),
                )
            })
            .await
            .map_err(|e| circuit_error_to_tool_error(e, "connect"))?;

        Ok(Self {
            service: Arc::new(service),
            resilience: Arc::new(McpResilience {
                list_timeout: resilience_config.list_timeout,
                call_timeout: resilience_config.call_timeout,
                retry_policy: resilience_config.retry_policy,
                breaker,
            }),
        })
    }

    /// The names the remote server advertises in `tools/list`.
    ///
    /// # Errors
    /// [`ToolError::Internal`] if the `tools/list` request fails (including
    /// after retries are exhausted, or when the circuit breaker is open).
    pub async fn list_tool_names(&self) -> Result<Vec<String>, ToolError> {
        let tools = self.resilience.list_all_tools(&self.service).await?;
        Ok(tools.into_iter().map(|t| t.name.into_owned()).collect())
    }

    /// Fetch the remote `tools/list` and wrap each entry as a registerable
    /// [`Tool`]. The wrappers share this connection and its resilience state
    /// (timeouts, retry policy, circuit breaker).
    ///
    /// # Errors
    /// [`ToolError::Internal`] if the `tools/list` request fails (including
    /// after retries are exhausted, or when the circuit breaker is open).
    pub async fn into_tools(self) -> Result<Vec<Box<dyn Tool>>, ToolError> {
        let remote = self.resilience.list_all_tools(&self.service).await?;
        let service = self.service;
        let resilience = self.resilience;
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
                    resilience: resilience.clone(),
                }) as Box<dyn Tool>
            })
            .collect())
    }
}

/// One attempt at the `initialize` handshake against `uri`, timeout-bounded.
/// Connect failures carry no side effects, so this is safe for
/// [`retry_with_backoff`] to call repeatedly.
async fn connect_once(
    uri: Arc<str>,
    bearer: Option<String>,
    connect_timeout: Duration,
) -> Result<RunningService<RoleClient, ()>, ToolError> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(uri);
    if let Some(token) = bearer {
        config = config.auth_header(token);
    }
    let transport = StreamableHttpClientTransport::from_config(config);
    match with_timeout(connect_timeout, ().serve(transport)).await {
        Ok(Ok(service)) => Ok(service),
        Ok(Err(e)) => Err(ToolError::Internal(anyhow::anyhow!(
            "MCP connect failed: {e}"
        ))),
        Err(_elapsed) => Err(ToolError::Internal(anyhow::anyhow!(
            "MCP connect timed out after {connect_timeout:?}"
        ))),
    }
}

impl McpResilience {
    /// `tools/list` through the shared breaker + retry policy + timeout — no
    /// side effects, so retrying is safe.
    async fn list_all_tools(
        &self,
        service: &RunningService<RoleClient, ()>,
    ) -> Result<Vec<rmcp::model::Tool>, ToolError> {
        self.breaker
            .call(|| {
                retry_with_backoff(
                    &self.retry_policy,
                    |_: &ToolError| true,
                    || list_all_tools_once(service, self.list_timeout),
                )
            })
            .await
            .map_err(|e| circuit_error_to_tool_error(e, "tools/list"))
    }

    /// `tools/call` through the shared breaker + timeout — deliberately
    /// **not** retried (see [`McpResilienceConfig`]'s doc comment): a partial
    /// failure reaching a tool with side effects must not be silently resent.
    async fn call_tool(
        &self,
        service: &RunningService<RoleClient, ()>,
        params: CallToolRequestParams,
    ) -> Result<rmcp::model::CallToolResult, ToolError> {
        self.breaker
            .call(|| call_tool_once(service, params, self.call_timeout))
            .await
            .map_err(|e| circuit_error_to_tool_error(e, "tools/call"))
    }
}

async fn list_all_tools_once(
    service: &RunningService<RoleClient, ()>,
    list_timeout: Duration,
) -> Result<Vec<rmcp::model::Tool>, ToolError> {
    match with_timeout(list_timeout, service.list_all_tools()).await {
        Ok(Ok(tools)) => Ok(tools),
        Ok(Err(e)) => Err(ToolError::Internal(anyhow::anyhow!(
            "tools/list failed: {e}"
        ))),
        Err(_elapsed) => Err(ToolError::Internal(anyhow::anyhow!(
            "tools/list timed out after {list_timeout:?}"
        ))),
    }
}

async fn call_tool_once(
    service: &RunningService<RoleClient, ()>,
    params: CallToolRequestParams,
    call_timeout: Duration,
) -> Result<rmcp::model::CallToolResult, ToolError> {
    match with_timeout(call_timeout, service.call_tool(params)).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(e)) => Err(ToolError::Internal(anyhow::anyhow!(
            "tools/call failed: {e}"
        ))),
        Err(_elapsed) => Err(ToolError::Internal(anyhow::anyhow!(
            "tools/call timed out after {call_timeout:?}"
        ))),
    }
}

/// A single remote MCP tool, presented as a local [`Tool`].
///
/// Its [`invoke`](Tool::invoke) forwards the arguments as a `tools/call` over
/// the shared connection and returns the remote result. It declares the
/// blanket [`Capability::Custom("mcp")`] ([`MCP_CAPABILITY`]) so the fused
/// runtime's cap-token/Cedar gate authorizes every MCP-boundary crossing
/// against the issuing cap-token's scope (ARD-478); the remote server may
/// enforce its own authorization on top.
pub struct RemoteMcpTool {
    id: ToolId,
    schema: ToolSchema,
    service: Arc<RunningService<RoleClient, ()>>,
    resilience: Arc<McpResilience>,
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
        let result = self.resilience.call_tool(&self.service, params).await?;

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
        &MCP_CAPS
    }
}
