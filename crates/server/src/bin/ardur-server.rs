//! `ardur-server` — the deployable binary.
//!
//! A thin shell over [`ardur_server`]: install tracing, read [`Config`] from the
//! environment, select the provider backend via `ARDUR_PROVIDER`, boot the
//! [`AppState`], and serve the router over a TCP listener until a SIGINT/SIGTERM
//! drains it.
#![forbid(unsafe_code)]

use ardur_provider_runtime::ModelId;
use ardur_provider_selector as provider_selector;
use ardur_server::{AppState, Config, build_router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(e) => {
            tracing::error!(error = %e, "configuration error");
            std::process::exit(1);
        }
    };

    // The live backend, selected by `ARDUR_PROVIDER` (default `anthropic`). An
    // unknown selector panics here at boot; a valid selection with a missing key
    // surfaces as an error and aborts startup (tests inject a stub instead).
    let provider = provider_selector::from_env(ModelId::new(&config.model))
        .map_err(|e| anyhow::anyhow!("building provider: {e}"))?;
    let provider_id = provider.id().0.clone();

    let state = AppState::boot(&config, provider)?;
    tracing::info!(
        data_dir = %state.data_dir().display(),
        bind = %config.bind_addr,
        model = %config.model,
        provider = %provider_id,
        budget_cents = config.cost_budget_cents,
        "ardur-server booted"
    );

    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .map_err(|e| anyhow::anyhow!("binding {}: {e}", config.bind_addr))?;
    tracing::info!(addr = %config.bind_addr, "listening for slack events");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| anyhow::anyhow!("server error: {e}"))?;

    // Graceful shutdown: fsync + close the durable journal before exit.
    if let Err(e) = state.journal().close().await {
        tracing::warn!(error = %e, "journal close failed during shutdown");
    }
    tracing::info!("ardur-server shut down cleanly");
    Ok(())
}

/// Install the process-wide tracing subscriber: env-driven (`RUST_LOG`, default
/// `info`), JSON-formatted when `ARDUR_LOG_FORMAT=json`.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let json = std::env::var("ARDUR_LOG_FORMAT").ok().as_deref() == Some("json");
    let fmt = tracing_subscriber::fmt().with_env_filter(filter);
    if json {
        fmt.json().init();
    } else {
        fmt.init();
    }
}

/// Resolve when the process receives SIGINT (Ctrl-C) or SIGTERM, so the server
/// can finish in-flight requests and exit cleanly.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received; draining in-flight requests");
}
