//! The runner — drives a [`Scenario`] against a live server and grades it.
//!
//! # Server contract (`POST /chat`, §4.0b)
//!
//! `ardur-server` exposes the synchronous chat surface this runner targets
//! (landed in PR #93). The runner POSTs one turn per prompt and grades the
//! consolidated reply:
//!
//! ```text
//! POST <base_url>/chat
//! Content-Type: application/json
//! { "message": "<prompt>", "session_id": "<optional uuid, for multi-turn>" }
//!
//! 200 OK
//! { "session_id": "<uuid the turn ran under; minted when omitted>",
//!   "reply": "<assistant text>",         // what the matchers grade
//!   "tokens": { "input": 120, "output": 30 }, // summed, graded by max_tokens
//!   "cost_usd": 0.0007,                   // graded by cost_under
//!   "tools_called": ["web_search"],       // graded by tool_called
//!   "receipt_id": "<uuid>" }
//! ```
//!
//! Status mapping: a `400` is a *failure* of the scenario (the server rejected
//! the request body — e.g. an empty message), surfaced as [`Outcome::Fail`]; a
//! `502` (the runtime rejected/failed the turn — cost gate, injection block,
//! provider error) and any other non-2xx surface as [`Outcome::Error`], as do
//! transport errors, timeouts, and malformed JSON — a flaky server marks the
//! scenario errored rather than aborting the whole run.
//!
//! Multi-turn scenarios omit `session_id` on the first turn, then thread the
//! server-minted id through every follow-up so the agent retains context. The
//! path is overridable via [`RunConfig::chat_path`] so a differently-named
//! endpoint needs no code change.

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
///
/// `stream` is deliberately never sent: the server rejects `stream: true` with
/// a `400`, and omitting it yields the consolidated reply the harness grades.
#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

/// The `200 OK` response body the server returns for a turn. `tokens` is a
/// nested `{ input, output }` object (summed for the `max_tokens` matcher);
/// `cost_usd` is graded by `cost_under`. Fields default so a partial body still
/// decodes — but the real server always populates them.
#[derive(Debug, Default, Deserialize)]
struct ChatResponse {
    /// The session the turn ran under — echoed back and threaded through any
    /// follow-up turns so a multi-turn scenario shares one session.
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    reply: String,
    /// Present ⇒ graded against `max_tokens` (input + output); absent ⇒ skipped.
    #[serde(default)]
    tokens: Option<Tokens>,
    #[serde(default)]
    cost_usd: Option<f64>,
    #[serde(default)]
    tools_called: Vec<String>,
}

/// The nested `tokens: { input, output }` object the server reports per turn.
#[derive(Debug, Default, Deserialize)]
struct Tokens {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
}

/// The `{ "error": "…" }` body the server returns on a `400` / `502`.
#[derive(Debug, Default, Deserialize)]
struct ChatError {
    #[serde(default)]
    error: String,
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
    let timeout = Duration::from_secs(scenario.timeout_secs.max(1));
    let started = Instant::now();

    // The initial prompt followed by any multi-turn follow-ups. The first turn
    // omits `session_id` (the server mints one); every follow-up threads the
    // server-returned id so the agent retains context. Matchers grade the last.
    let mut prompts = vec![scenario.prompt.clone()];
    prompts.extend(scenario.follow_ups.iter().cloned());

    // A short closure to stamp an outcome with the scenario's identity + timing.
    let result_with = |outcome: Outcome, reply: String| ScenarioResult {
        id: scenario.id.clone(),
        description: scenario.description.clone(),
        outcome,
        reply,
        duration_ms: started.elapsed().as_millis(),
    };

    let mut session_id: Option<String> = None;
    let mut last: ChatResponse = ChatResponse::default();
    for prompt in &prompts {
        let body = ChatRequest {
            message: prompt,
            session_id: session_id.as_deref(),
        };
        let resp = client.post(&url).timeout(timeout).json(&body).send().await;
        match resp {
            Ok(r) if r.status().is_success() => match r.json::<ChatResponse>().await {
                Ok(parsed) => {
                    // Capture the session for follow-ups (the first turn mints it).
                    if !parsed.session_id.is_empty() {
                        session_id = Some(parsed.session_id.clone());
                    }
                    last = parsed;
                }
                Err(e) => {
                    return result_with(
                        Outcome::Error {
                            message: format!("decoding /chat response: {e}"),
                        },
                        String::new(),
                    );
                }
            },
            Ok(r) => {
                let status = r.status();
                // Pull the server's `{ "error": … }` detail when present.
                let detail = r
                    .json::<ChatError>()
                    .await
                    .map(|e| e.error)
                    .unwrap_or_default();
                let outcome = match status.as_u16() {
                    // The server rejected the request body itself (empty message,
                    // unsupported `stream`, …) — a scenario *failure*, not an error.
                    400 => Outcome::Fail {
                        reasons: vec![format!("bad_request: {detail}")],
                    },
                    // The runtime rejected or failed the turn (cost gate, injection
                    // block, provider error) — an error of the exchange.
                    502 => Outcome::Error {
                        message: format!("runtime: {detail}"),
                    },
                    _ => Outcome::Error {
                        message: if detail.is_empty() {
                            format!("server returned HTTP {status}")
                        } else {
                            format!("server returned HTTP {status}: {detail}")
                        },
                    },
                };
                return result_with(outcome, String::new());
            }
            Err(e) => {
                return result_with(
                    Outcome::Error {
                        message: format!("POST {url} failed: {e}"),
                    },
                    String::new(),
                );
            }
        }
    }

    // The server reports input + output token counts; sum them for the
    // `max_tokens` matcher. An absent `tokens` object ⇒ usage-graded matcher
    // skipped (the field is `None`).
    let tokens_total = last.tokens.as_ref().map(|t| (t.input + t.output) as u32);
    let reasons = grade(
        &scenario.expected,
        &last.reply,
        tokens_total,
        last.cost_usd,
        &last.tools_called,
        scenario.max_tokens,
    );
    let outcome = if reasons.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Fail { reasons }
    };

    result_with(outcome, last.reply)
}
