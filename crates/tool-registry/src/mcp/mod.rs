//! The §6.0 Phase-2 MCP bridge: ardur's [`ToolRegistry`](crate::ToolRegistry)
//! exposed over the Model Context Protocol, and remote MCP servers consumed back
//! as [`Tool`](crate::Tool)s — both built on the official `rmcp` Rust SDK.
//!
//! Two halves, mirror images of each other:
//!
//! - [`ArdurMcpServer`] (server side) — an `rmcp` [`ServerHandler`] wrapping a
//!   shared [`ToolRegistry`]. Each registered tool becomes a `tools/list` entry,
//!   and each `tools/call` is dispatched to the matching local
//!   [`Tool::invoke`](crate::Tool::invoke). Mount it under a Streamable-HTTP
//!   transport (the `crates/server` axum routes do this) to serve ardur's tools
//!   to any MCP client.
//!
//! - [`RemoteMcpToolset`] (client side) — connects to a configured MCP URL over
//!   Streamable HTTP, fetches its `tools/list`, and wraps each remote tool as a
//!   local [`Tool`] whose `invoke` forwards a `tools/call` over the wire. The
//!   wrapped tools register alongside in-process ones, indistinguishable to the
//!   runtime.
//!
//! [`ServerHandler`]: rmcp::ServerHandler

mod bearer;
mod client;
mod server;

pub use bearer::{bearer_token_allowed, extract_bearer_token};
pub use client::{MCP_CAPABILITY, McpResilienceConfig, RemoteMcpTool, RemoteMcpToolset};
pub use server::ArdurMcpServer;
