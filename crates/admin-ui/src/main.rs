//! `ardur-admin` — entry point.
//!
//! Parses the CLI, optionally connects to Qdrant (read-only), builds the
//! router, and serves the dashboard. See `lib.rs` and `README.md`.

use std::net::SocketAddr;

use clap::Parser;

use ardur_admin::auth::BasicAuth;
use ardur_admin::build_router;
use ardur_admin::config::Cli;
use ardur_admin::state::{AppState, MemorySource};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let cli = Cli::parse();

    // Optional, read-only Qdrant connection for the memory endpoint.
    let memory = match &cli.qdrant_url {
        Some(url) => {
            let client = qdrant_client::Qdrant::from_url(url)
                .build()
                .map_err(|e| anyhow::anyhow!("connecting to qdrant at {url}: {e}"))?;
            tracing::info!(%url, collection = %cli.qdrant_collection, "memory endpoint enabled");
            Some(MemorySource {
                client,
                collection: cli.qdrant_collection.clone(),
            })
        }
        None => None,
    };

    let mut state = AppState::new(cli.journal_dir.clone(), cli.receipt_store.clone());
    if let Some(m) = memory {
        state = state.with_memory(m);
    }
    if let Some(user_pass) = &cli.basic_auth {
        state = state.with_basic_auth(BasicAuth::from_user_pass(user_pass));
        tracing::info!("HTTP Basic auth enabled");
    }

    let app = build_router(state.shared());

    // Default to localhost for security; only bind to 0.0.0.0 when explicitly requested
    let bind_host = std::env::var("ARDUR_ADMIN_BIND")
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let addr = match bind_host.parse::<std::net::IpAddr>() {
        Ok(ip) => SocketAddr::from((ip, cli.port)),
        Err(_) => {
            tracing::warn!("Invalid ARDUR_ADMIN_BIND, falling back to 127.0.0.1");
            SocketAddr::from(([127, 0, 0, 1], cli.port))
        }
    };
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        %addr,
        journal_dir = %cli.journal_dir.display(),
        receipt_store = %cli.receipt_store.display(),
        "ardur-admin listening (read-only)"
    );
    axum::serve(listener, app).await?;
    Ok(())
}
