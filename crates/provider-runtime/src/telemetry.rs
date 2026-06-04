//! OpenTelemetry GenAI wiring — the OTLP exporter + tracing subscriber that
//! ships the `gen_ai.*` provider spans (emitted by
//! [`InstrumentedProvider`](crate::InstrumentedProvider)) to any OTLP-native
//! backend (Langfuse, Phoenix, Arize, Jaeger, …).
//!
//! This module is *only* the transport: it installs the process-wide tracing
//! subscriber with an [`tracing_opentelemetry`] layer bridged onto an OTLP gRPC
//! exporter. The spans themselves — and their GenAI semantic-convention
//! attributes — are opened by the [`InstrumentedProvider`](crate::InstrumentedProvider)
//! decorator regardless of whether an exporter is installed; with OTel disabled
//! the same spans simply flow to whatever `tracing` subscriber the host (CLI /
//! server / a test) already set.
//!
//! GenAI semantic conventions: <https://opentelemetry.io/docs/specs/semconv/gen-ai/>.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The standard OTLP gRPC endpoint a local collector listens on.
const DEFAULT_OTLP_ENDPOINT: &str = "http://localhost:4317";
/// The `service.name` resource attribute reported to the backend when the
/// environment does not override it.
const DEFAULT_SERVICE_NAME: &str = "ardur-agent";

/// The installed tracer provider, retained for [`shutdown_genai_tracing`] to
/// flush on exit. `None` until [`init_genai_tracing`] runs with telemetry
/// enabled.
static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();
/// Set once [`init_genai_tracing`] has installed the subscriber, so a second
/// call is a no-op rather than a double-install panic.
static INITIALIZED: OnceLock<()> = OnceLock::new();

/// Knobs for the OTel exporter, read from the environment by
/// [`TelemetryConfig::from_env`] or built explicitly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelemetryConfig {
    /// Whether to stand up the OTLP exporter at all. When `false`,
    /// [`init_genai_tracing`] is a no-op (the host's own subscriber stays in
    /// place and provider spans still flow to it).
    pub enabled: bool,
    /// The OTLP gRPC endpoint to export spans to.
    pub otlp_endpoint: String,
    /// The `service.name` resource attribute.
    pub service_name: String,
    /// The `service.version` resource attribute.
    pub service_version: String,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            otlp_endpoint: DEFAULT_OTLP_ENDPOINT.to_string(),
            service_name: DEFAULT_SERVICE_NAME.to_string(),
            service_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

impl TelemetryConfig {
    /// Read the config from the environment, falling back to [`Default`] for any
    /// unset variable:
    ///
    /// - `ARDUR_OTEL_ENABLED` — `1`/`true`/`yes`/`on` (case-insensitive) enables.
    /// - `OTEL_EXPORTER_OTLP_ENDPOINT` — the standard OTLP endpoint variable.
    /// - `OTEL_SERVICE_NAME` — the standard service-name variable.
    ///
    /// `service_version` is always the crate's compile-time version.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// The parsing core behind [`from_env`](Self::from_env), reading each variable
    /// through `lookup` so the precedence/blank-handling logic is testable without
    /// mutating the process environment (which edition 2024 makes `unsafe`, and
    /// `#![forbid(unsafe_code)]` then bans). A value that is absent *or blank*
    /// falls back to the [`Default`].
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let defaults = Self::default();
        let non_empty = |key: &str| {
            lookup(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let enabled = non_empty("ARDUR_OTEL_ENABLED")
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(defaults.enabled);
        Self {
            enabled,
            otlp_endpoint: non_empty("OTEL_EXPORTER_OTLP_ENDPOINT")
                .unwrap_or(defaults.otlp_endpoint),
            service_name: non_empty("OTEL_SERVICE_NAME").unwrap_or(defaults.service_name),
            service_version: defaults.service_version,
        }
    }
}

/// Everything that can go wrong standing up the GenAI tracing pipeline.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// The OTLP span exporter could not be built (bad endpoint, transport init).
    #[error("building the OTLP span exporter: {0}")]
    Exporter(String),
    /// The tracing subscriber could not be installed as the global default.
    #[error("installing the tracing subscriber: {0}")]
    Subscriber(String),
}

/// Install the GenAI tracing pipeline: an OTLP gRPC span exporter feeding a
/// [`tracing_opentelemetry`] layer, stacked under the process-wide `tracing`
/// subscriber alongside a console `fmt` layer and an [`EnvFilter`] (`RUST_LOG`,
/// default `info`).
///
/// Idempotent: a second call (or a call after the host already installed a
/// subscriber) returns `Ok(())` without re-installing. When
/// [`TelemetryConfig::enabled`] is `false`, this is a no-op — the caller's own
/// subscriber stays in place and provider spans still flow to it.
///
/// # Errors
///
/// Returns [`TelemetryError::Exporter`] if the OTLP exporter cannot be built,
/// or [`TelemetryError::Subscriber`] if the subscriber cannot be installed
/// (e.g. a non-OTel global subscriber was already set by the host).
pub fn init_genai_tracing(config: TelemetryConfig) -> Result<(), TelemetryError> {
    if !config.enabled {
        // Telemetry off: leave whatever subscriber the host installed. The
        // provider spans are emitted unconditionally; they just route to the
        // host's subscriber instead of an OTLP exporter.
        return Ok(());
    }
    if INITIALIZED.get().is_some() {
        return Ok(());
    }

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&config.otlp_endpoint)
        .build()
        .map_err(|e| TelemetryError::Exporter(e.to_string()))?;

    let resource = Resource::builder()
        .with_service_name(config.service_name.clone())
        .with_attribute(KeyValue::new(
            "service.version",
            config.service_version.clone(),
        ))
        .build();

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("ardur-provider-runtime");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(otel_layer)
        .try_init()
        .map_err(|e| TelemetryError::Subscriber(e.to_string()))?;

    // Register globally so non-tracing OTel call sites (and a clean shutdown)
    // see the same provider, then retain it for the flush on exit.
    opentelemetry::global::set_tracer_provider(provider.clone());
    let _ = TRACER_PROVIDER.set(provider);
    let _ = INITIALIZED.set(());
    Ok(())
}

/// Flush and shut down the OTLP exporter so buffered spans are delivered before
/// the process exits. A no-op when telemetry was never initialized.
pub fn shutdown_genai_tracing() {
    if let Some(provider) = TRACER_PROVIDER.get() {
        if let Err(e) = provider.shutdown() {
            tracing::warn!(error = %e, "flushing OpenTelemetry spans on shutdown");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `lookup` that reads from a fixed `(key, value)` table — stands in for the
    /// process environment so the parsing logic is exercised without the `unsafe`
    /// env mutators that `#![forbid(unsafe_code)]` bans.
    fn lookup<'a>(table: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            table
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn config_from_env_defaults() {
        // Nothing set: every field falls back to its default.
        let config = TelemetryConfig::from_lookup(lookup(&[]));
        assert!(!config.enabled, "telemetry defaults to off");
        assert_eq!(config.otlp_endpoint, DEFAULT_OTLP_ENDPOINT);
        assert_eq!(config.service_name, DEFAULT_SERVICE_NAME);
        assert_eq!(config.service_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn config_from_env_reads_overrides() {
        let config = TelemetryConfig::from_lookup(lookup(&[
            ("ARDUR_OTEL_ENABLED", "true"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4317"),
            ("OTEL_SERVICE_NAME", "ardur-test"),
        ]));
        assert!(config.enabled, "ARDUR_OTEL_ENABLED=true enables");
        assert_eq!(config.otlp_endpoint, "http://collector:4317");
        assert_eq!(config.service_name, "ardur-test");
        // A blank override is treated as unset and falls back to the default.
        let blank = TelemetryConfig::from_lookup(lookup(&[("OTEL_SERVICE_NAME", "   ")]));
        assert_eq!(blank.service_name, DEFAULT_SERVICE_NAME);
    }

    #[tokio::test]
    async fn init_twice_is_idempotent() {
        // The batch exporter connects lazily, so init succeeds without a live
        // collector; a second call must short-circuit rather than panic on a
        // double global-subscriber install.
        let config = TelemetryConfig {
            enabled: true,
            ..TelemetryConfig::default()
        };
        assert!(init_genai_tracing(config.clone()).is_ok(), "first init");
        assert!(init_genai_tracing(config).is_ok(), "second init is a no-op");
    }
}
