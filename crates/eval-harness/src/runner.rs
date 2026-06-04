//! The runner — drives a [`Scenario`] against a live server and grades it.
//!
//! # Assumed server contract
//!
//! `ardur-server` does not (yet) expose a public chat endpoint — its HTTP
//! surface is the Slack webhook (`POST /slack/events`), `GET /healthz`, and the
//! optional MCP routes. Rather than block on that, the runner targets a small,
//! documented contract the server can grow into:
//!
//! ```text
//! POST <base_url>/chat
//! Content-Type: application/json
//! { "message": "<prompt>", "session_id": "<optional, for multi-turn>" }
//!
//! 200 OK
//! { "reply": "<assistant text>",
//!   "tokens": 42,                  // optional usage, graded by max_tokens
//!   "cost_usd": 0.0007,            // optional cost, graded by cost_under
//!   "tools_called": ["web_search"] // optional, graded by tool_called
//! }
//! ```
//!
//! When that endpoint lands the harness works unchanged; until then the runner
//! is exercised against a `wiremock` stand-in (see the crate tests). The path
//! is overridable via [`RunConfig::chat_path`] so a differently-named endpoint
//! needs no code change.

use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::scenario::{Expected, Scenario};

/// Runtime knobs for a run: where the server is and which path to POST to.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Base URL of the server under test, e.g. `http://localhost:8080`.
    pub base_url: String,
    /// The chat endpoint path appended to `base_url`. Defaults to `/chat`.
    pub chat_path: String,
}

impl RunConfig {
    /// A config targeting `base_url` with the default `/chat` path.
    pub fn new(base_url: impl Into<String>) -> Self {
        RunConfig {
            base_url: base_url.into(),
            chat_path: "/chat".to_string(),
        }
    }
}

/// The request body the runner POSTs for each turn.
#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

/// The response body the runner expects back. Only `reply` is required; the
/// usage/cost/tool fields are optional and absent ⇒ that matcher is skipped
/// (with a recorded reason) rather than failing hard.
#[derive(Debug, Default, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    reply: String,
    #[serde(default)]
    tokens: Option<u32>,
    #[serde(default)]
    cost_usd: Option<f64>,
    #[serde(default)]
    tools_called: Vec<String>,
}

/// The verdict for a scenario.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Outcome {
    /// Every populated matcher held.
    Pass,
    /// At least one matcher failed; `reasons` lists each failure.
    Fail {
        /// One human-readable line per failed matcher.
        reasons: Vec<String>,
    },
    /// The exchange itself errored (transport, timeout, non-2xx, bad JSON).
    Error {
        /// What went wrong.
        message: String,
    },
}

impl Outcome {
    /// True only for [`Outcome::Pass`].
    pub fn is_pass(&self) -> bool {
        matches!(self, Outcome::Pass)
    }
}

/// The graded result of running one scenario, ready for any output format.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioResult {
    /// The scenario id.
    pub id: String,
    /// Its description (echoed for the report).
    pub description: String,
    /// Pass / Fail / Error.
    pub outcome: Outcome,
    /// The final assistant reply (truncated-safe; full text retained).
    pub reply: String,
    /// Wall-clock milliseconds the exchange took.
    pub duration_ms: u128,
}

/// Grade a `reply` (plus optional server-reported usage/cost/tools) against the
/// scenario's [`Expected`] block. Returns the list of failure reasons — empty
/// ⇒ pass. Pure and synchronous, so it is unit-testable without HTTP.
pub fn grade(
    expected: &Expected,
    reply: &str,
    tokens: Option<u32>,
    cost_usd: Option<f64>,
    tools_called: &[String],
    max_tokens: u32,
) -> Vec<String> {
    let mut reasons = Vec::new();

    for needle in &expected.contains {
        if !reply.contains(needle.as_str()) {
            reasons.push(format!("expected reply to contain {needle:?}"));
        }
    }
    for needle in &expected.not_contains {
        if reply.contains(needle.as_str()) {
            reasons.push(format!("expected reply to NOT contain {needle:?}"));
        }
    }
    if let Some(pattern) = &expected.regex {
        match Regex::new(pattern) {
            Ok(re) => {
                if !re.is_match(reply) {
                    reasons.push(format!("expected reply to match regex {pattern:?}"));
                }
            }
            Err(e) => reasons.push(format!("invalid regex {pattern:?}: {e}")),
        }
    }
    if let Some(tool) = &expected.tool_called {
        if !tools_called.iter().any(|t| t == tool) {
            reasons.push(format!(
                "expected tool {tool:?} to be called (server reported {tools_called:?})"
            ));
        }
    }
    if let Some(limit) = expected.cost_under {
        match cost_usd {
            Some(c) if c >= limit => {
                reasons.push(format!("expected cost < {limit} but turn cost {c}"));
            }
            Some(_) => {}
            None => reasons.push(format!(
                "expected cost < {limit} but server reported no cost_usd"
            )),
        }
    }
    if max_tokens > 0 {
        if let Some(used) = tokens {
            if used > max_tokens {
                reasons.push(format!(
                    "expected <= {max_tokens} tokens but turn used {used}"
                ));
            }
        }
    }

    reasons
}

/// Run one scenario against the server: POST the prompt (and any follow-ups on
/// a shared session id), grade the final reply, and return a [`ScenarioResult`].
///
/// Transport, timeout, non-2xx, and malformed-JSON failures surface as
/// [`Outcome::Error`] rather than panicking — a flaky server marks a scenario
/// errored, not the whole run aborted.
pub async fn run_scenario(
    client: &reqwest::Client,
    config: &RunConfig,
    scenario: &Scenario,
) -> ScenarioResult {
    let url = format!(
        "{}/{}",
        config.base_url.trim_end_matches('/'),
        config.chat_path.trim_start_matches('/')
    );
    let session_id = scenario.id.clone();
    let timeout = Duration::from_secs(scenario.timeout_secs.max(1));
    let started = Instant::now();

    // The initial prompt followed by any multi-turn follow-ups, all on one
    // session id so the agent retains context. The matchers grade the last.
    let mut prompts = vec![scenario.prompt.clone()];
    prompts.extend(scenario.follow_ups.iter().cloned());

    let mut last: ChatResponse = ChatResponse::default();
    for prompt in &prompts {
        let body = ChatRequest {
            message: prompt,
            session_id: Some(&session_id),
        };
        let resp = client.post(&url).timeout(timeout).json(&body).send().await;
        match resp {
            Ok(r) if r.status().is_success() => match r.json::<ChatResponse>().await {
                Ok(parsed) => last = parsed,
                Err(e) => {
                    return ScenarioResult {
                        id: scenario.id.clone(),
                        description: scenario.description.clone(),
                        outcome: Outcome::Error {
                            message: format!("decoding /chat response: {e}"),
                        },
                        reply: String::new(),
                        duration_ms: started.elapsed().as_millis(),
                    };
                }
            },
            Ok(r) => {
                let status = r.status();
                return ScenarioResult {
                    id: scenario.id.clone(),
                    description: scenario.description.clone(),
                    outcome: Outcome::Error {
                        message: format!("server returned HTTP {status}"),
                    },
                    reply: String::new(),
                    duration_ms: started.elapsed().as_millis(),
                };
            }
            Err(e) => {
                return ScenarioResult {
                    id: scenario.id.clone(),
                    description: scenario.description.clone(),
                    outcome: Outcome::Error {
                        message: format!("POST {url} failed: {e}"),
                    },
                    reply: String::new(),
                    duration_ms: started.elapsed().as_millis(),
                };
            }
        }
    }

    let reasons = grade(
        &scenario.expected,
        &last.reply,
        last.tokens,
        last.cost_usd,
        &last.tools_called,
        scenario.max_tokens,
    );
    let outcome = if reasons.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Fail { reasons }
    };

    ScenarioResult {
        id: scenario.id.clone(),
        description: scenario.description.clone(),
        outcome,
        reply: last.reply,
        duration_ms: started.elapsed().as_millis(),
    }
}
