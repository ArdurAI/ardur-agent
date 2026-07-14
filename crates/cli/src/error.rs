//! The CLI's single typed-error surface.
//!
//! Every fallible operation the `ardur` binary performs returns [`CliError`].
//! The variants keep the failure domains distinct so the REPL can report a
//! config problem differently from a dead provider or an exhausted budget.

use ardur_cap_token::CapTokenError;
use ardur_cost_gate::AdmissionError;
use ardur_provider_runtime::ProviderError;
use ardur_runtime::RuntimeError;

/// All ways a CLI operation can fail.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The config file could not be parsed (its contents are malformed).
    #[error("invalid config: {0}")]
    Config(String),

    /// An I/O failure — reading config, building the async runtime, or a
    /// non-EOF line-editor read.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The runtime rejected or failed a turn.
    #[error("runtime error: {0}")]
    Runtime(#[from] RuntimeError),

    /// A provider call failed.
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),

    /// A capability-token operation failed.
    #[error("cap-token error: {0}")]
    CapToken(#[from] CapTokenError),

    /// Persistent session state could not be set up: a `~/.ardur/` key, the
    /// Cedar bundle, the session cap-token, or the fused-runtime build failed.
    #[error("state error: {0}")]
    State(String),
}

/// Webhook operator errors (§9.7) surface as CLI state errors, preserving the
/// operator-facing message (refusals, not-found, signing-key resolution).
impl From<ardur_webhook::WebhookError> for CliError {
    fn from(e: ardur_webhook::WebhookError) -> Self {
        CliError::State(e.to_string())
    }
}

/// Cost-admission failures surface as runtime failures: admitting a turn is part
/// of running it. A denied or exhausted budget maps onto
/// [`RuntimeError::CostCeilingExceeded`]; anything else is an internal runtime
/// fault.
// TODO §2.1 Phase 2: give cost-gate denials a first-class `CliError::Budget`
// variant so the REPL can render the available/required cents distinctly.
impl From<AdmissionError> for CliError {
    fn from(e: AdmissionError) -> Self {
        match e {
            AdmissionError::BudgetExhausted { .. }
            | AdmissionError::PolicyDenied(_)
            | AdmissionError::ProviderNotAllowed(_) => {
                CliError::Runtime(RuntimeError::CostCeilingExceeded)
            }
            other => CliError::Runtime(RuntimeError::Internal(anyhow::Error::new(other))),
        }
    }
}
