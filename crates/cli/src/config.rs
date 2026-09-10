//! The CLI's on-disk configuration: the Anthropic credentials, the default
//! model, and the session's starting budget.
//!
//! Config lives at `~/.ardur/config.toml`. A missing file is not an error —
//! [`Config::load`] falls back to [`Config::default`] so a fresh checkout runs
//! out of the box. Phase 1 reads a deliberately small, flat subset of TOML
//! (`key = "string"` / `key = integer`, `#` comments) with a hand-rolled
//! reader rather than pulling a parser; the full schema lands in Phase 2.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::CliError;

/// The default model the chat REPL completes against when config omits one.
pub const DEFAULT_MODEL: &str = "claude-opus-4-8";

/// The default starting budget, in whole US cents (the §2.1 blueprint's 1000c).
pub const DEFAULT_BUDGET_CENTS: u64 = 1000;

/// The resolved CLI configuration for a `ardur chat` session.
///
/// `api_key` is intentionally excluded from [`Serialize`] output so a debug log
/// of the effective config never leaks the credential.
#[derive(Clone, Debug, Serialize)]
pub struct Config {
    /// The Anthropic API key. Empty by default — the Phase-1 provider stub
    /// rejects an empty key, surfacing the misconfiguration early.
    #[serde(skip_serializing)]
    pub api_key: String,
    /// The model id completions default to.
    pub model: String,
    /// The session's starting budget, in cents.
    pub budget_cents: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: DEFAULT_MODEL.to_string(),
            budget_cents: DEFAULT_BUDGET_CENTS,
        }
    }
}

impl Config {
    /// The default config path: `~/.ardur/config.toml`. `None` if the home
    /// directory cannot be resolved.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        // `HOME` on unix, `USERPROFILE` on Windows — enough for Phase 1 without
        // pulling a directories crate.
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(|home| PathBuf::from(home).join(".ardur").join("config.toml"))
    }

    /// Load config from `path` (or the [`default_path`](Self::default_path) when
    /// `None`). A missing file yields [`Config::default`]; a present-but-malformed
    /// file is a [`CliError::Config`].
    ///
    /// Environment variables override config-file values, matching the server's
    /// precedence: `ARDUR_MODEL` overrides `model`.
    pub fn load(path: Option<PathBuf>) -> Result<Self, CliError> {
        let Some(path) = path.or_else(Self::default_path) else {
            return Ok(Self::from_env_overlay(Self::default()));
        };
        let config = match std::fs::read_to_string(&path) {
            Ok(contents) => Self::from_toml_str(&contents)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(CliError::Io(e)),
        };
        Ok(Self::from_env_overlay(config))
    }

    /// Apply environment-variable overrides on top of a loaded (or default)
    /// config. Currently honours `ARDUR_MODEL`.
    fn from_env_overlay(mut config: Self) -> Self {
        if let Ok(model) = std::env::var("ARDUR_MODEL") {
            if !model.trim().is_empty() {
                config.model = model;
            }
        }
        if let Ok(budget) = std::env::var("ARDUR_CLI_BUDGET_CENTS") {
            if let Ok(cents) = budget.trim().parse::<u64>() {
                config.budget_cents = cents;
            }
        }
        config
    }

    /// Parse the flat Phase-1 TOML subset, overlaying any recognized keys onto
    /// the defaults.
    // TODO §2.1 Phase 2: replace this hand-rolled reader with a real TOML parser
    // and a richer schema (per-provider sections, model aliases, ceilings).
    fn from_toml_str(contents: &str) -> Result<Self, CliError> {
        let mut config = Self::default();
        for (lineno, raw) in contents.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(CliError::Config(format!(
                    "line {}: expected `key = value`, got `{raw}`",
                    lineno + 1
                )));
            };
            let key = key.trim();
            let value = value.trim().trim_matches('"');
            match key {
                "api_key" => config.api_key = value.to_string(),
                "model" => config.model = value.to_string(),
                "budget_cents" => {
                    config.budget_cents = value.parse().map_err(|_| {
                        CliError::Config(format!(
                            "line {}: budget_cents must be an integer, got `{value}`",
                            lineno + 1
                        ))
                    })?;
                }
                // Unknown keys are tolerated so a Phase-2 config still loads under
                // a Phase-1 binary.
                _ => {}
            }
        }
        Ok(config)
    }
}

/// A redacted, structured snapshot of the effective config for a debug log —
/// the credential is never included.
#[must_use]
pub fn redacted_summary(config: &Config, source: &Path) -> serde_json::Value {
    serde_json::json!({
        "source": source.display().to_string(),
        "model": config.model,
        "budget_cents": config.budget_cents,
        "api_key_present": !config.api_key.is_empty(),
    })
}
