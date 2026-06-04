//! The §6.0 MCP HTTP surface: ardur's tool registry exposed over the official
//! `rmcp` Streamable-HTTP transport, fronted by a bearer-token gate.
//!
//! [`build_mcp_router`] assembles a self-contained axum [`Router`] mounting the
//! [`ArdurMcpServer`] at `<prefix>/{server_name}` for the three Streamable-HTTP
//! methods (GET setup, POST JSON-RPC dispatch, DELETE session close). Every
//! request must carry `Authorization: Bearer <token>` matching the configured
//! allowlist — checked in constant time — or it is rejected with `401` before
//! reaching the transport.
//!
//! The router carries no [`AppState`](crate::AppState) dependency, so
//! [`build_router`](crate::build_router) merges it into the top-level router when
//! the MCP surface is enabled.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};

use ardur_tool_registry::{
    ArdurMcpServer, EchoTool, HealthCheckTool, RemoteMcpToolset, Tool, ToolRegistry,
    bearer_token_allowed, extract_bearer_token,
};

/// The two example tools the server advertises over MCP: a trivial `echo`
/// round-trip and a `health_check` reporting uptime, provider, and memory
/// backend.
///
/// `provider` is the selected provider id and `memory_backend` labels the memory
/// store, both surfaced by `health_check`.
#[must_use]
pub fn example_registry(
    provider: impl Into<String>,
    memory_backend: impl Into<String>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    // Registration only fails on a duplicate id; these two are distinct and
    // fixed, so the inserts cannot fail.
    registry
        .register(Box::new(EchoTool::new()))
        .expect("echo id is unique");
    registry
        .register(Box::new(HealthCheckTool::new(provider, memory_backend)))
        .expect("health_check id is unique");
    registry
}

/// **§6.0.** Connect each configured remote MCP server and collect the tools it
/// advertises, ready to register into the runtime's registry. `servers` is the
/// parsed `ARDUR_MCP_REMOTE_SERVERS` list (`name`, `url`); a server that fails
/// to connect or list is logged and skipped rather than aborting boot, so one
/// dead remote does not take the agent down.
///
/// Must be awaited on a long-lived runtime (the binary's `#[tokio::main]`): the
/// returned tools hold the live MCP client sessions, whose background drivers
/// run on the runtime this is awaited on.
pub async fn connect_remote_tools(servers: &[(String, String)]) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    for (name, url) in servers {
        match RemoteMcpToolset::connect(url.clone(), None).await {
            Ok(toolset) => match toolset.into_tools().await {
                Ok(mut remote) => {
                    tracing::info!(
                        server = %name,
                        url = %url,
                        count = remote.len(),
                        "connected remote MCP toolset"
                    );
                    tools.append(&mut remote);
                }
                Err(e) => tracing::warn!(
                    server = %name, url = %url, error = %e,
                    "listing remote MCP tools failed; skipping this server"
                ),
            },
            Err(e) => tracing::warn!(
                server = %name, url = %url, error = %e,
                "connecting remote MCP server failed; skipping this server"
            ),
        }
    }
    tools
}

/// **§6.0.** Assemble the tool registry the fused runtime invokes: the local
/// tools ([`example_registry`]) plus every tool from the configured remote MCP
/// servers (`ARDUR_MCP_REMOTE_SERVERS`). A remote tool whose id collides with an
/// already-registered one is logged and skipped (first registration wins).
pub async fn assemble_tool_registry(
    provider: impl Into<String>,
    memory_backend: impl Into<String>,
    servers: &[(String, String)],
) -> ToolRegistry {
    let mut registry = example_registry(provider, memory_backend);
    for tool in connect_remote_tools(servers).await {
        let id = tool.id();
        if let Err(e) = registry.register(tool) {
            tracing::warn!(tool = %id, error = %e, "skipping remote MCP tool with a conflicting id");
        }
    }
    registry
}

/// Build the bearer-gated MCP router: `registry` exposed at
/// `<path_prefix>/{server_name}`, admitting only requests whose bearer token is
/// in `bearer_tokens`.
///
/// Generic over the ambient router state `S` so it merges into the top-level
/// `Router<Arc<AppState>>`; the MCP routes themselves carry no such state (the
/// bearer middleware supplies its own).
pub fn build_mcp_router<S>(
    registry: Arc<ToolRegistry>,
    bearer_tokens: Vec<String>,
    path_prefix: &str,
) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let handler = ArdurMcpServer::new(registry);
    // `disable_allowed_hosts` lifts the transport's default loopback-only DNS-
    // rebinding guard: this endpoint is meant for remote, programmatic MCP
    // clients across arbitrary hosts, and the bearer-token allowlist is the
    // actual security boundary. Operators front it with their own ingress/TLS.
    let config = StreamableHttpServerConfig::default().disable_allowed_hosts();
    let service: StreamableHttpService<ArdurMcpServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            config,
        );

    // The transport dispatches GET/POST/DELETE itself; one service route per
    // named server. `:server_name` (axum 0.7 path-param syntax) lets a
    // deployment expose several logical servers on one host (all currently
    // backed by the same registry).
    let route = format!("{}/:server_name", path_prefix.trim_end_matches('/'));

    Router::new()
        .route_service(&route, service)
        .layer(middleware::from_fn_with_state(
            Arc::new(bearer_tokens),
            require_bearer,
        ))
}

/// axum middleware: admit a request only if its `Authorization: Bearer <token>`
/// is in the allowlist (constant-time match), else `401`.
async fn require_bearer(
    State(allowlist): State<Arc<Vec<String>>>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match extract_bearer_token(presented) {
        Some(token) if bearer_token_allowed(token, &allowlist) => next.run(request).await,
        _ => (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response(),
    }
}
