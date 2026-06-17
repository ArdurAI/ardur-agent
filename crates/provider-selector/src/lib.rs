//! ardur-provider-selector — boot-time selection of a §3.x model backend.
//!
//! The CLI and the server both build exactly one [`Provider`] at boot and hand
//! it to the FusedRuntime. Historically that provider was hard-coded to
//! Anthropic. This crate is the single seam that reads the `ARDUR_PROVIDER`
//! environment variable and dispatches to the right backend's `from_env`
//! constructor, returning a boxed `Arc<dyn Provider>`:
//!
//! | `ARDUR_PROVIDER` | backend                              | crate                       |
//! |------------------|--------------------------------------|-----------------------------|
//! | `anthropic`      | [`AnthropicProvider`] (default)      | `ardur-provider-runtime`    |
//! | `openrouter`     | `OpenRouterProvider`                 | `ardur-provider-openrouter` |
//! | `openai-compat`  | `OpenAiCompatProvider`               | `ardur-provider-openai-compat` |
//! | `ollama`         | `OllamaProvider` (local **or** cloud)| `ardur-provider-ollama`     |
//! | `codex`          | `CodexProvider` (subprocess wrap)    | `ardur-provider-codex`      |
//! | `claude-cli`     | `ClaudeCliProvider` (subprocess wrap)| `ardur-provider-claude-cli` |
//!
//! (`claude-cli` also answers to the alias `claude-subscription`.)
//!
//! Parsing is case-insensitive; an unset (or empty) value selects the default,
//! `anthropic`. An unrecognized value is returned as a [`ProviderError`] whose
//! message lists the supported values, so a typo aborts boot cleanly without a
//! panic or silent fallback.
//!
//! The credentialed backends ([`ProviderKind::Anthropic`],
//! [`ProviderKind::OpenRouter`], [`ProviderKind::OpenAiCompat`]) can still fail
//! *after* a valid selection when their key is absent — that surfaces as
//! `Err(ProviderError)`, distinct from the panic on an unknown selector. The
//! credential-free backends
//! ([`ProviderKind::Ollama`], [`ProviderKind::Codex`], [`ProviderKind::ClaudeCli`])
//! never fail.
//!
//! # Why a standalone crate
//!
//! The peer backends each depend on `ardur-provider-runtime` (they implement
//! its `Provider` trait). A selector that depends on all of them therefore
//! cannot live *inside* `provider-runtime` without closing a dependency cycle. A
//! leaf crate sitting above the concrete providers is the only acyclic home.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;
use std::sync::Arc;

use ardur_provider_claude_cli::ClaudeCliProvider;
use ardur_provider_codex::CodexProvider;
use ardur_provider_ollama::OllamaProvider;
use ardur_provider_openai_compat::OpenAiCompatProvider;
use ardur_provider_openrouter::OpenRouterProvider;
use ardur_provider_runtime::AnthropicProvider;

// Re-exported so callers can name the `from_env`/`select` argument and return
// types without also depending on `ardur-provider-runtime` directly. These also
// bring the names into this module's scope for the dispatch below.
pub use ardur_provider_runtime::{ModelId, Provider, ProviderError};

/// The environment variable that selects the provider backend at boot.
pub const SELECTOR_ENV: &str = "ARDUR_PROVIDER";

/// A recognized provider backend, parsed from [`SELECTOR_ENV`].
///
/// The `&str` spellings round-trip through [`ProviderKind::parse`] /
/// [`ProviderKind::as_str`] and match the `id()` each backend reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    /// Anthropic Messages API (§3.1) — the default when unset.
    Anthropic,
    /// OpenRouter HTTP gateway (§3.2).
    OpenRouter,
    /// Generic OpenAI-compatible HTTP backend (§12.5).
    OpenAiCompat,
    /// Ollama local daemon or hosted cloud (§3.3).
    Ollama,
    /// OpenAI Codex CLI subscription, wrapped as a subprocess (§3.3b).
    Codex,
    /// Claude Code CLI subscription, wrapped as a subprocess (§3.3c). Also
    /// selected by the alias `claude-subscription`.
    ClaudeCli,
}

impl ProviderKind {
    /// The selection used when [`SELECTOR_ENV`] is unset or empty.
    pub const DEFAULT: ProviderKind = ProviderKind::Anthropic;

    /// Every recognized selector spelling, in selection order — the canonical
    /// list surfaced in the unknown-value error.
    pub const ALL: [ProviderKind; 7] = [
        ProviderKind::Anthropic,
        ProviderKind::OpenRouter,
        ProviderKind::OpenAiCompat,
        ProviderKind::Ollama,
        ProviderKind::Codex,
        ProviderKind::ClaudeCli,
        ProviderKind::OpenAiCompat, // openai alias
    ];

    /// The canonical lowercase spelling — matches each backend's `id()`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::OpenRouter => "openrouter",
            ProviderKind::OpenAiCompat => "openai-compat",
            ProviderKind::Ollama => "ollama",
            ProviderKind::Codex => "codex",
            ProviderKind::ClaudeCli => "claude-cli",
        }
    }

    /// Parse a selector value, case-insensitively and ignoring surrounding
    /// whitespace. Returns [`UnknownProvider`] for an unrecognized value.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownProvider`] carrying the offending value when it matches
    /// none of [`ProviderKind::ALL`].
    pub fn parse(raw: &str) -> Result<Self, UnknownProvider> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Ok(ProviderKind::Anthropic),
            "openrouter" => Ok(ProviderKind::OpenRouter),
            "openai-compat" | "openai_compat" | "openai" => Ok(ProviderKind::OpenAiCompat),
            "ollama" => Ok(ProviderKind::Ollama),
            "codex" => Ok(ProviderKind::Codex),
            "claude-cli" | "claude-subscription" => Ok(ProviderKind::ClaudeCli),
            _ => Err(UnknownProvider(raw.to_string())),
        }
    }

    /// Resolve an optional raw selector value to a kind: `None` (or an
    /// all-whitespace value) selects [`ProviderKind::DEFAULT`].
    ///
    /// # Errors
    ///
    /// Returns [`UnknownProvider`] when a non-empty value is unrecognized.
    pub fn resolve(raw: Option<&str>) -> Result<Self, UnknownProvider> {
        match raw {
            None => Ok(ProviderKind::DEFAULT),
            Some(v) if v.trim().is_empty() => Ok(ProviderKind::DEFAULT),
            Some(v) => ProviderKind::parse(v),
        }
    }

    /// Build the live backend for this kind, reading its own configuration from
    /// the environment. `model` is the default model the credentialed/HTTP
    /// backends pin completions to; the Ollama backend ignores it (its model is
    /// carried in `OllamaConfig`).
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when a credentialed backend's key is missing
    /// ([`ProviderKind::Anthropic`] / [`ProviderKind::OpenRouter`] /
    /// [`ProviderKind::OpenAiCompat`] → [`ProviderError::Unauthorized`]).
    /// The credential-free backends
    /// ([`ProviderKind::Ollama`] / [`ProviderKind::Codex`] /
    /// [`ProviderKind::ClaudeCli`]) never fail.
    pub fn build(self, model: ModelId) -> Result<Arc<dyn Provider>, ProviderError> {
        let provider: Arc<dyn Provider> = match self {
            ProviderKind::Anthropic => Arc::new(AnthropicProvider::from_env(model)?),
            ProviderKind::OpenRouter => Arc::new(OpenRouterProvider::from_env(model)?),
            ProviderKind::OpenAiCompat => Arc::new(OpenAiCompatProvider::from_env(model)?),
            ProviderKind::Ollama => Arc::new(OllamaProvider::from_env()),
            ProviderKind::Codex => Arc::new(CodexProvider::from_env(model)),
            ProviderKind::ClaudeCli => Arc::new(ClaudeCliProvider::from_env(model)),
        };
        Ok(provider)
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An `ARDUR_PROVIDER` value that matched no known backend.
///
/// Its [`Display`](fmt::Display) lists the supported values, so the boot panic
/// it triggers tells the operator exactly what to set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownProvider(pub String);

impl fmt::Display for UnknownProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown {SELECTOR_ENV} value {:?}: supported values are \
             anthropic (default), openrouter, openai-compat, ollama, codex, claude-cli",
            self.0
        )
    }
}

impl std::error::Error for UnknownProvider {}

/// Resolve an explicit selector value to a live boxed provider.
///
/// `None` (or an all-whitespace value) selects [`ProviderKind::DEFAULT`]. This
/// is the env-free core that [`from_env`] wraps — the CLI/server boot tests call
/// it directly with a fixed value so they need not mutate process environment.
///
/// # Errors
///
/// Returns [`ProviderError`] when the selected (valid) backend cannot be built
/// from the environment — e.g. a missing `ANTHROPIC_API_KEY`,
/// `OPENROUTER_API_KEY`, or `OPENAI_COMPAT_API_KEY` /
/// `OPENAI_API_KEY` yields [`ProviderError::Unauthorized`]. An unrecognized
/// selector is returned as [`ProviderError::InvalidRequest`] with a message that
/// lists all supported values.
pub fn select(selector: Option<&str>, model: ModelId) -> Result<Arc<dyn Provider>, ProviderError> {
    let kind = ProviderKind::resolve(selector)
        .map_err(|e| ProviderError::InvalidRequest(format!("invalid provider selection: {e}")))?;
    kind.build(model)
}

/// Read [`SELECTOR_ENV`] (`ARDUR_PROVIDER`) and resolve it to a live boxed
/// provider, pinning the credentialed/HTTP backends to `model`.
///
/// This is the boot entry point the CLI and server call in place of the old
/// hard-coded `AnthropicProvider::from_env`.
///
/// # Errors
///
/// Returns [`ProviderError`] when the selected backend cannot be built from the
/// environment (e.g. a missing API key), or when `ARDUR_PROVIDER` is set to an
/// unrecognized value.
pub fn from_env(model: ModelId) -> Result<Arc<dyn Provider>, ProviderError> {
    let raw = std::env::var(SELECTOR_ENV).ok();
    select(raw.as_deref(), model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> ModelId {
        ModelId::new("test-model")
    }

    // --- Parsing / resolution (no environment, no credentials) ---

    #[test]
    fn default_selects_anthropic() {
        // An unset selector resolves to the Anthropic default.
        assert_eq!(
            ProviderKind::resolve(None).unwrap(),
            ProviderKind::Anthropic
        );
        // As does an empty / whitespace value.
        assert_eq!(
            ProviderKind::resolve(Some("   ")).unwrap(),
            ProviderKind::Anthropic
        );
    }

    #[test]
    fn anthropic_explicit_selects_anthropic() {
        for v in ["anthropic", "Anthropic", "ANTHROPIC", "  anthropic  "] {
            assert_eq!(
                ProviderKind::resolve(Some(v)).unwrap(),
                ProviderKind::Anthropic,
                "{v:?} should select anthropic"
            );
        }
    }

    #[test]
    fn openrouter_selects_openrouter() {
        for v in ["openrouter", "OpenRouter", "OPENROUTER"] {
            assert_eq!(
                ProviderKind::resolve(Some(v)).unwrap(),
                ProviderKind::OpenRouter,
                "{v:?} should select openrouter"
            );
        }
    }

    #[test]
    fn openai_compat_selects_openai_compat() {
        for v in [
            "openai-compat",
            "OpenAI-Compat",
            "OPENAI_COMPAT",
            "openai",
            "  openai-compat  ",
        ] {
            assert_eq!(
                ProviderKind::resolve(Some(v)).unwrap(),
                ProviderKind::OpenAiCompat,
                "{v:?} should select openai-compat"
            );
        }
    }

    #[test]
    fn ollama_selects_ollama_local() {
        // With no OLLAMA_API_KEY in the ambient env, the Ollama backend defaults
        // to a local daemon and is built infallibly. (CI runs without the key.)
        let provider = select(Some("ollama"), model()).expect("ollama is infallible");
        assert_eq!(provider.id().0, "ollama");
    }

    #[test]
    fn codex_selects_codex() {
        // Codex wraps the local `codex` CLI; from_env is infallible (it does not
        // probe the binary until a turn runs), so selection always succeeds.
        let provider = select(Some("codex"), model()).expect("codex is infallible");
        assert_eq!(provider.id().0, "codex");
    }

    #[test]
    fn claude_cli_selects_claude_cli() {
        // The Claude CLI backend wraps the local `claude` binary; from_env is
        // infallible (no API key, no probe until a turn runs). Both the canonical
        // spelling and the `claude-subscription` alias resolve to it.
        for v in [
            "claude-cli",
            "Claude-CLI",
            "CLAUDE-CLI",
            "claude-subscription",
            "  claude-subscription  ",
        ] {
            assert_eq!(
                ProviderKind::resolve(Some(v)).unwrap(),
                ProviderKind::ClaudeCli,
                "{v:?} should select claude-cli"
            );
        }
        let provider = select(Some("claude-cli"), model()).expect("claude-cli is infallible");
        assert_eq!(provider.id().0, "claude-cli");
    }

    #[test]
    fn unknown_provider_returns_helpful_error() {
        let result = select(Some("mistral"), model());
        assert!(result.is_err(), "unknown provider should error");
        let message = match result {
            Err(e) => format!("{}", e),
            Ok(_) => panic!("expected error"),
        };
        assert!(message.contains("supported values are"), "{message}");
        assert!(message.contains("openai-compat"), "{message}");
    }

    #[test]
    fn provider_id_matches_selection() {
        // The reported id() round-trips with the selector spelling — checked for
        // the credential-free backends (the credentialed ones need a key to
        // construct, so their spelling is asserted via as_str()).
        for kind in [
            ProviderKind::Ollama,
            ProviderKind::Codex,
            ProviderKind::ClaudeCli,
        ] {
            let provider = kind.build(model()).expect("infallible backend");
            assert_eq!(
                provider.id().0,
                kind.as_str(),
                "{kind} id() should equal its selector spelling"
            );
        }
        // The full set still round-trips through parse <-> as_str.
        for kind in ProviderKind::ALL {
            assert_eq!(ProviderKind::parse(kind.as_str()).unwrap(), kind);
        }
    }
}
