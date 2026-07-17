//! `ardur-server` — the deployable binary.
//!
//! A thin shell over [`ardur_server`]: install tracing, read [`Config`] from the
//! environment, select the provider backend via `ARDUR_PROVIDER`, boot the
//! [`AppState`], and serve the router over a TCP listener until a SIGINT/SIGTERM
//! drains it.
#![forbid(unsafe_code)]

use std::sync::Arc;

use ardur_provider_runtime::{InstrumentedProvider, ModelId, TelemetryConfig};
use ardur_provider_runtime::{init_genai_tracing, shutdown_genai_tracing};
use ardur_provider_selector as provider_selector;
use ardur_server::{AppState, Config, MemoryBackend, assemble_tool_registry, build_router};

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
    // unknown selector returns a clean error; a valid selection with a missing
    // key surfaces as an error and aborts startup (tests inject a stub instead).
    // Wrap it in `InstrumentedProvider` so every dispatch emits a `provider.send`
    // span with the GenAI semconv attributes — for free, regardless of backend.
    let provider = provider_selector::from_env(ModelId::new(&config.model))
        .map_err(|e| anyhow::anyhow!("building provider: {e}"))?;
    let provider = InstrumentedProvider::wrap(provider);
    let provider_id = provider.id().0.clone();

    // §6.0: assemble the tool registry the runtime invokes — the local tools plus
    // any remote MCP toolsets from `ARDUR_MCP_REMOTE_SERVERS`. Connecting happens
    // here on the long-lived `#[tokio::main]` runtime so the remote MCP client
    // sessions stay driven for the life of the process.
    let memory_label = match config.memory_backend {
        MemoryBackend::InMemory => "in-memory",
        MemoryBackend::Qdrant => "qdrant",
        MemoryBackend::Hybrid => "hybrid",
    };
    // ARD-457: `builtin_tool_opts()` maps the operator's fail-closed opt-ins
    // (`ARDUR_ENABLE_SHELL_TOOL` + allowlist, `ARDUR_ENABLE_HTTP_TOOL`,
    // `ARDUR_FILE_TOOL_ROOT`) to the hardened built-ins registered below. With no
    // opt-ins set it grants nothing, so the default boot is unchanged.
    let tools = Arc::new(
        assemble_tool_registry(
            provider_id.clone(),
            memory_label,
            &config.skills_dirs,
            &config.mcp_remote_servers,
            config.builtin_tool_opts(),
        )
        .await,
    );

    let state = AppState::boot(&config, provider, tools).await?;
    tracing::info!(
        data_dir = %state.data_dir().display(),
        bind = %config.bind_addr,
        model = %config.model,
        provider = %provider_id,
        budget_cents = config.cost_budget_cents,
        slack_enabled = config.slack_enabled,
        channel_matrix = config.channel_matrix,
        channel_discord = config.channel_discord,
        channel_telegram = config.channel_telegram,
        channel_email = config.channel_email,
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

    // Third channel: Discord. Same opt-in + async-connect shape as Matrix.
    if config.channel_discord {
        let discord_config = ardur_channel_discord::DiscordConfig::from_env()
            .map_err(|e| anyhow::anyhow!("reading discord config: {e}"))?;
        let discord = ardur_channel_discord::DiscordChannel::new(discord_config)
            .await
            .map_err(|e| anyhow::anyhow!("connecting discord channel: {e}"))?;
        state.attach_discord(std::sync::Arc::new(discord)).await;
        tracing::info!("discord channel attached and connecting");
    }

    // Fourth channel: Telegram.
    if config.channel_telegram {
        let telegram_config = ardur_channel_telegram::TelegramConfig::from_env()
            .map_err(|e| anyhow::anyhow!("reading telegram config: {e}"))?;
        let telegram = ardur_channel_telegram::TelegramChannel::new(telegram_config)
            .await
            .map_err(|e| anyhow::anyhow!("connecting telegram channel: {e}"))?;
        state.attach_telegram(std::sync::Arc::new(telegram));
        tracing::info!("telegram channel attached and polling");
    }

    // Fifth channel: email.
    if config.channel_email {
        let email_config = ardur_channel_email::EmailConfig::from_env()
            .map_err(|e| anyhow::anyhow!("reading email config: {e}"))?;
        let email = ardur_channel_email::EmailChannel::new(email_config)
            .await
            .map_err(|e| anyhow::anyhow!("connecting email channel: {e}"))?;
        state.attach_email(std::sync::Arc::new(email));
        tracing::info!("email channel attached and polling");
    }

    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .map_err(|e| anyhow::anyhow!("binding {}: {e}", config.bind_addr))?;
    if config.slack_enabled {
        tracing::info!(addr = %config.bind_addr, "listening for HTTP + Slack events");
    } else {
        tracing::info!(addr = %config.bind_addr, "listening for HTTP requests (Slack disabled)");
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| anyhow::anyhow!("server error: {e}"))?;

    // Graceful shutdown: signal the worker, then fsync + close the durable
    // journal before exit.
    state.shutdown();
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
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "failed to install Ctrl-C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to install SIGTERM handler");
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received; draining in-flight requests");
}
