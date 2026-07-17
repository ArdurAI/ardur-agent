//! The scenario format — a declarative, YAML-authored eval case.
//!
//! A [`Scenario`] pairs a `prompt` (what to send the agent under test) with an
//! [`Expected`] block of matchers (how to grade the reply) and a set of run
//! limits (`max_tokens` / `max_turns` / `timeout_secs`). The format is
//! intentionally Tau-Bench-flavoured: one self-contained file per case, fully
//! data-driven, no Rust changes needed to add a scenario.
//!
//! ```yaml
//! id: scenario-001
//! description: Agent should answer factual question
//! prompt: "What is the capital of France?"
//! expected:
//!   contains: ["Paris"]
//!   not_contains: ["London"]
//!   regex: "(?i)\\bparis\\b"
//!   tool_called: web_search
//!   cost_under: 0.01
//! max_tokens: 100
//! max_turns: 1
//! timeout_secs: 30
//! ```

use std::path::Path;

use serde::{Deserialize, Serialize};

/// A single evaluation case: a prompt, the matchers that grade its reply, and
/// the run limits the harness enforces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scenario {
    /// Stable identifier, also the report key. Conventionally the file stem.
    pub id: String,
    /// Human-readable one-liner: what the case asserts.
    #[serde(default)]
    pub description: String,
    /// The user message sent to the agent under test.
    pub prompt: String,
    /// The grading matchers applied to the reply.
    #[serde(default)]
    pub expected: Expected,
    /// Upper bound on completion tokens the agent may spend (informational +
    /// graded when the server reports usage). `0` / absent ⇒ unbounded.
    #[serde(default)]
    pub max_tokens: u32,
    /// Conversation turns the scenario drives. `1` (the default) is a single
    /// prompt/response; richer multi-turn cases set this higher.
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Per-scenario wall-clock budget for the HTTP exchange.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Optional follow-up prompts for multi-turn scenarios. Each is sent after
    /// the initial `prompt`, reusing the same session id so the agent retains
    /// context. The matchers grade the *final* reply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub follow_ups: Vec<String>,
}

fn default_max_turns() -> u32 {
    1
}

fn default_timeout_secs() -> u64 {
    30
}

/// The declarative matcher block. Every populated matcher must hold for the
/// scenario to pass; an empty block trivially passes (useful for smoke cases
/// that only assert the server answered at all).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Expected {
    /// Each substring must appear in the reply (case-sensitive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contains: Vec<String>,
    /// None of these substrings may appear in the reply (case-sensitive).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_contains: Vec<String>,
    /// The reply must match this regular expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
    /// The agent must have invoked this tool during the turn (matched against
    /// the server-reported `tools_called` list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_called: Option<String>,
    /// The turn's reported cost (USD) must be strictly below this threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_under: Option<f64>,
}

/// Parse errors surfaced when loading a scenario file.
#[derive(Debug, thiserror::Error)]
pub enum ScenarioError {
    /// The file could not be read.
    #[error("reading scenario {path}: {source}")]
    Io {
        /// The offending path.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The YAML did not deserialize into a [`Scenario`].
    #[error("parsing scenario {path}: {source}")]
    Parse {
        /// The offending path.
        path: String,
        /// The underlying YAML error.
        source: serde_yaml::Error,
    },
}

impl Scenario {
    /// Parse a [`Scenario`] from a YAML string.
    pub fn from_yaml(yaml: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
    }

    /// Serialize this scenario back to YAML (used by `ardur-eval new`).
    pub fn to_yaml(&self) -> Result<String, serde_yaml::Error> {
        serde_yaml::to_string(self)
    }

    /// Load a [`Scenario`] from a YAML file on disk.
    pub fn load(path: &Path) -> Result<Self, ScenarioError> {
        let text = std::fs::read_to_string(path).map_err(|source| ScenarioError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_yaml(&text).map_err(|source| ScenarioError::Parse {
            path: path.display().to_string(),
            source,
        })
    }

    /// Load every `*.yaml` / `*.yml` scenario in a directory, sorted by path so
    /// the report order is deterministic.
    pub fn load_dir(dir: &Path) -> Result<Vec<Self>, ScenarioError> {
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .map_err(|source| ScenarioError::Io {
                path: dir.display().to_string(),
                source,
            })?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("yaml") | Some("yml")
                )
            })
            .collect();
        paths.sort();
        paths.iter().map(|p| Self::load(p)).collect()
    }

    /// A blank scenario scaffold for `ardur-eval new --id <id>`.
    pub fn scaffold(id: &str) -> Self {
        Scenario {
            id: id.to_string(),
            description: "Describe what this scenario asserts.".to_string(),
            prompt: "Replace with the prompt to send the agent.".to_string(),
            expected: Expected {
                contains: vec!["expected substring".to_string()],
                ..Default::default()
            },
            max_tokens: 0,
            max_turns: 1,
            timeout_secs: 30,
            follow_ups: Vec::new(),
        }
    }
}
