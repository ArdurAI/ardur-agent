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

    // ARD-421: Security check — non-loopback bind requires --basic-auth unless
    // --unsafe-bind is explicitly set.
    let bind_is_loopback = is_loopback(&cli.bind_addr);
    if !bind_is_loopback && cli.basic_auth.is_none() && !cli.unsafe_bind {
        anyhow::bail!(
            "Non-loopback bind (--bind-addr {}) requires --basic-auth for security.\n\
             Use --unsafe-bind to override this check (NOT recommended).",
            cli.bind_addr
        );
    }

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

    let ip: std::net::IpAddr = cli
        .bind_addr
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --bind-addr `{}`: {e}", cli.bind_addr))?;
    let addr = SocketAddr::from((ip, cli.port));
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

/// Returns true if the address is loopback (127.0.0.1 or ::1).
fn is_loopback(addr: &str) -> bool {
    match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(std::net::IpAddr::V6(v6)) => v6.is_loopback(),
        Err(_) => false,
    }
}
