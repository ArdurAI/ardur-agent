//! `ardur-server` — the deployable binary.
//!
//! A thin shell over [`ardur_server`]: install tracing, read [`Config`] from the
//! environment, select the provider backend via `ARDUR_PROVIDER`, boot the
//! [`AppState`], and serve the router over a TCP listener until a SIGINT/SIGTERM
//! drains it.
#![forbid(unsafe_code)]

use ardur_provider_runtime::{InstrumentedProvider, ModelId, TelemetryConfig};
use ardur_provider_runtime::{init_genai_tracing, shutdown_genai_tracing};
use ardur_provider_selector as provider_selector;
use ardur_server::{AppState, Config, build_router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // When `ARDUR_OTEL_ENABLED=true`, stand up the OpenTelemetry GenAI pipeline
    // (OTLP exporter + the layered subscriber) so provider spans export to any
    // OTLP backend; otherwise install the plain console subscriber. Only one
    // process-wide subscriber may be set, so these are mutually exclusive.
    let telemetry = TelemetryConfig::from_env();
    if telemetry.enabled {
        init_genai_tracing(telemetry.clone())
            .map_err(|e| anyhow::anyhow!("initializing OpenTelemetry tracing: {e}"))?;
        tracing::info!(
            otlp_endpoint = %telemetry.otlp_endpoint,
            service_name = %telemetry.service_name,
            "OpenTelemetry GenAI tracing enabled"
        );
    } else {
        init_tracing();
    }

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
    // Wrap it in `InstrumentedProvider` so every dispatch emits a `provider.send`
    // span with the GenAI semconv attributes — for free, regardless of backend.
    let provider = provider_selector::from_env(ModelId::new(&config.model))
        .map_err(|e| anyhow::anyhow!("building provider: {e}"))?;
    let provider = InstrumentedProvider::wrap(provider);
    let provider_id = provider.id().0.clone();

    let state = AppState::boot(&config, provider)?;
    tracing::info!(
        data_dir = %state.data_dir().display(),
        bind = %config.bind_addr,
        model = %config.model,
        provider = %provider_id,
        budget_cents = config.cost_budget_cents,
        channel_matrix = config.channel_matrix,
        "ardur-server booted"
    );

    // Second channel: when enabled, connect the Matrix bot and wire its sync +
    // forwarding alongside Slack. Construction is async (client build + session
    // restore), so it happens here in the runtime rather than inside `boot`.
    if config.channel_matrix {
        let matrix_config = ardur_channel_matrix::MatrixConfig::from_env()
            .map_err(|e| anyhow::anyhow!("reading matrix config: {e}"))?;
        let matrix = ardur_channel_matrix::MatrixChannel::new(matrix_config)
            .await
            .map_err(|e| anyhow::anyhow!("connecting matrix channel: {e}"))?;
        let matrix = std::sync::Arc::new(matrix);
        let matrix_user = matrix.user_id().to_string();
        state.attach_matrix(matrix);
        tracing::info!(user = %matrix_user, "matrix channel attached and syncing");
    }

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
    // Flush any buffered OpenTelemetry spans before exit (a no-op when telemetry
    // was never initialized).
    shutdown_genai_tracing();
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
