//! CLI configuration — the *only* way ardur-admin is configured.
//!
//! No environment variables are read (except `ARDUR_ADMIN_BIND`) and
//! ardur-server's `Config` is never touched: the operator points the binary at
//! the directories ardur-server persists to and (optionally) its Qdrant
//! endpoint.

use std::net::IpAddr;
use std::path::PathBuf;

use clap::Parser;

/// Default dashboard port — deliberately distinct from ardur-server's typical
/// `8080` so the two can run side by side on one host.
pub const DEFAULT_PORT: u16 = 8090;

/// The default bind address — loopback only. Use `--unsafe-bind` to listen on
/// a non-loopback interface.
pub const DEFAULT_BIND: &str = "127.0.0.1";

/// `ardur-admin` — a read-only observability dashboard over an ardur-server
/// deployment's persisted journals, receipts, and (optionally) memory.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "ardur-admin",
    about = "Read-only observability dashboard for ardur-server (journals, receipts, memory).",
    long_about = "Reads ardur-server's persisted artifacts directly and serves them over an \
HTTP dashboard. Strictly read-only: it never writes to any journal, receipt, or memory store. \
Intended for a trusted local or private network (see README for the security model)."
)]
pub struct Cli {
    /// Directory containing the session journals — the same path ardur-server
    /// writes journals under. Sessions live at
    /// `<journal-dir>/sessions/<session-id>/journal.jsonl`.
    #[arg(long, value_name = "PATH")]
    pub journal_dir: PathBuf,

    /// Directory holding the receipt chain. The chain file is
    /// `<receipt-store>/chain.jsonl`; a path that is itself a `.jsonl` file is
    /// also accepted and used verbatim.
    #[arg(long, value_name = "PATH")]
    pub receipt_store: PathBuf,

    /// Optional Qdrant gRPC URL. When set, `/api/memory/recent` scrolls the
    /// memory collection read-only; when unset, that endpoint reports the
    /// feature as disabled.
    #[arg(long, value_name = "URL")]
    pub qdrant_url: Option<String>,

    /// The Qdrant collection to read when `--qdrant-url` is set (defaults to the
    /// same `ardur_memory` collection the durable store uses).
    #[arg(long, value_name = "NAME", default_value = "ardur_memory")]
    pub qdrant_collection: String,

    /// Port to serve the dashboard on.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,

    /// Optional HTTP Basic credentials as `user:pass`. When set, every endpoint
    /// requires them. Meant for a light gate on a shared network — not a
    /// substitute for real auth (see README).
    #[arg(long, value_name = "USER:PASS")]
    pub basic_auth: Option<String>,

    /// Optional comma-separated Bearer token(s). When set, every endpoint
    /// accepts `Authorization: Bearer <token>` matching any of them, checked
    /// with the same fail-closed, constant-time, length-bounded comparison
    /// `ardur-server` uses for its own admin bearer gate. Preferred over
    /// `--basic-auth` for anything reachable beyond loopback. The
    /// `ARDUR_ADMIN_BEARER_TOKENS` environment variable overrides the default
    /// when the flag is absent, so the token need not appear in shell history.
    #[arg(
        long,
        value_name = "TOKEN[,TOKEN...]",
        env = "ARDUR_ADMIN_BEARER_TOKENS"
    )]
    pub bearer_tokens: Option<String>,

    /// The bind address. Defaults to `127.0.0.1` (loopback only). The
    /// `ARDUR_ADMIN_BIND` environment variable overrides the default when the
    /// flag is absent.
    ///
    /// **Security:** non-loopback binds require `--basic-auth` or the server
    /// will refuse to start. Use `--unsafe-bind` to acknowledge the risk of
    /// exposing the dashboard on a non-loopback interface.
    #[arg(long, value_name = "ADDR", env = "ARDUR_ADMIN_BIND")]
    pub bind_addr: Option<String>,

    /// Acknowledge the security risk of binding to a non-loopback address.
    /// Without this flag, a non-loopback `--bind-addr` without `--basic-auth`
    /// is a startup error.
    #[arg(long)]
    pub unsafe_bind: bool,
}

/// Parse a comma-separated token list, trimming whitespace and dropping empty
/// entries. Mirrors `ardur-server`'s own `parse_csv` for `ARDUR_ADMIN_BEARER_TOKENS`
/// so the two admin-bearer configs behave identically.
#[must_use]
pub fn parse_bearer_tokens(value: Option<&str>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Resolve the effective bind address from the CLI flag, env var, or default.
///
/// # Errors
/// Returns a descriptive error if the address fails to parse.
pub fn resolve_bind_addr(cli: &Cli) -> Result<IpAddr, String> {
    let raw = cli.bind_addr.as_deref().unwrap_or(DEFAULT_BIND);
    raw.parse::<IpAddr>()
        .map_err(|e| format!("invalid bind address `{raw}`: {e}"))
}

/// Returns `true` if `addr` is a loopback address.
#[must_use]
pub fn is_loopback(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Validate the bind configuration. A non-loopback bind without `--basic-auth`
/// and without `--unsafe-bind` is rejected.
///
/// # Errors
/// Returns a human-readable error explaining the security risk.
pub fn validate_bind(cli: &Cli, addr: &IpAddr) -> Result<(), String> {
    if is_loopback(addr) {
        return Ok(());
    }
    if cli.basic_auth.is_some() {
        return Ok(());
    }
    if !parse_bearer_tokens(cli.bearer_tokens.as_deref()).is_empty() {
        return Ok(());
    }
    if cli.unsafe_bind {
        tracing::warn!(
            "ardur-admin is binding to a non-loopback address ({addr}) without HTTP Basic or \
             Bearer auth. Anyone on the network can read journals, receipts, and memory. This is \
             strongly discouraged."
        );
        return Ok(());
    }
    Err(format!(
        "refusing to bind to non-loopback address {addr} without --basic-auth or --bearer-tokens. \
         The admin dashboard exposes sessions, journals, receipts, costs, and memory. \
         Re-run with --bearer-tokens TOKEN (preferred) or --basic-auth USER:PASS to protect it, \
         or pass --unsafe-bind to acknowledge the risk."
    ))
}
