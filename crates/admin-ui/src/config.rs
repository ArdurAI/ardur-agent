//! CLI configuration — the *only* way ardur-admin is configured.
//!
//! No environment variables are read and ardur-server's `Config` is never
//! touched: the operator points the binary at the directories ardur-server
//! persists to and (optionally) its Qdrant endpoint.

use std::path::PathBuf;

use clap::Parser;

/// Default dashboard port — deliberately distinct from ardur-server's typical
/// `8080` so the two can run side by side on one host.
pub const DEFAULT_PORT: u16 = 8090;

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

    /// Address to bind the dashboard on (default: 127.0.0.1 — loopback only).
    /// Override with `ARDUR_ADMIN_BIND` env var. Non-loopback addresses
    /// require `--basic-auth` unless `--unsafe-bind` is also set.
    #[arg(
        long,
        value_name = "ADDR",
        env = "ARDUR_ADMIN_BIND",
        default_value = "127.0.0.1"
    )]
    pub bind_addr: String,

    /// Allow non-loopback bind without `--basic-auth` (NOT recommended).
    #[arg(long)]
    pub unsafe_bind: bool,

    /// Optional HTTP Basic credentials as `user:pass`. When set, every endpoint
    /// requires them. Meant for a light gate on a shared network — not a
    /// substitute for real auth (see README).
    #[arg(long, value_name = "USER:PASS")]
    pub basic_auth: Option<String>,
}
