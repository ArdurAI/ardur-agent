//! `ardur-admin` — entry point.
//!
//! Parses the CLI, optionally connects to Qdrant (read-only), builds the
//! router, and serves the dashboard. See `lib.rs` and `README.md`.

use std::net::SocketAddr;

use clap::Parser;

use ardur_admin::approvals::ServerConfig;
use ardur_admin::auth::{BasicAuth, BearerAuth};
use ardur_admin::build_router;
use ardur_admin::config::{
    Cli, parse_bearer_tokens, resolve_bind_addr, validate_approvals_auth, validate_bind,
};
use ardur_admin::state::{AppState, MemorySource};
use ardur_cedar_policy::{CedarPolicyBundle, PolicyBundle, PolicySource};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let cli = Cli::parse();

    // Resolve + validate the bind address before doing anything else: a
    // non-loopback bind without --basic-auth (and without --unsafe-bind) is
    // a startup error.
    let bind_ip = resolve_bind_addr(&cli).map_err(anyhow::Error::msg)?;
    validate_bind(&cli, &bind_ip).map_err(anyhow::Error::msg)?;
    // Refuse an unauthenticated approvals proxy — clap's `requires` already
    // guarantees --server-url and --server-admin-token appear together, but
    // says nothing about admin-ui's own auth gate.
    validate_approvals_auth(&cli).map_err(anyhow::Error::msg)?;

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
    let bearer_tokens = parse_bearer_tokens(cli.bearer_tokens.as_deref());
    if !bearer_tokens.is_empty() {
        tracing::info!(count = bearer_tokens.len(), "Bearer auth enabled");
        state = state.with_bearer_auth(BearerAuth::from_tokens(bearer_tokens));
    }
    if let Some(path) = &cli.policy_bundle {
        let policies = CedarPolicyBundle::load(PolicySource::File(path.clone()))
            .map_err(|e| anyhow::anyhow!("compiling cedar policy at {}: {e}", path.display()))?;
        tracing::info!(
            path = %path.display(),
            policy_count = policies.policy_count(),
            "Trust Center policy debugger enabled"
        );
        state = state.with_policies(policies);
    }
    if let (Some(url), Some(token)) = (&cli.server_url, &cli.server_admin_token) {
        tracing::info!(server_url = %url, "Approvals proxy enabled");
        state = state.with_approvals_server(ServerConfig::new(url.clone(), token.clone()));
    }

    let app = build_router(state.shared());

    let addr = SocketAddr::from((bind_ip, cli.port));
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
