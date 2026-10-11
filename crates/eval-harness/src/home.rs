//! Serial paired-home evaluation. Only authoritative replies enter text grading.

use crate::runner::{Outcome, ScenarioResult, grade};
use crate::scenario::{Expected, Scenario};
use crate::transcript::Transcript;
use home_client::{DeviceCommand, HomeClient, execute_device, fresh_client_nonce};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// A scenario with an explicit bot or room destination.
#[derive(Debug, Deserialize)]
pub struct HomeScenario {
    /// Reused text matchers and turn limits.
    #[serde(flatten)]
    pub scenario: Scenario,
    /// The home owns the chosen bot's model, tools, and continuing conversation.
    pub target: Target,
}
/// Explicit destinations prevent accidental fan-out.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Target {
    /// One continuing bot, selected by id or unambiguous name.
    Bot {
        /// Bot id or name.
        bot: String,
    },
    /// A room may admit several runs; grade their ordered text replies together.
    Room {
        /// Room id.
        room_id: String,
        /// Optional explicit room thread; otherwise resolve it from rooms/list.
        #[serde(default)]
        thread_id: Option<String>,
    },
}
impl HomeScenario {
    /// Parse and validate without exposing source text in errors.
    pub fn from_yaml(text: &str) -> Result<Self, &'static str> {
        let value: serde_yaml::Value =
            serde_yaml::from_str(text).map_err(|_| "Malformed scenario.")?;
        let map = value.as_mapping().ok_or("Expected a scenario object.")?;
        let fields = [
            "id",
            "description",
            "prompt",
            "expected",
            "max_tokens",
            "max_turns",
            "timeout_secs",
            "follow_ups",
            "target",
        ];
        if map
            .keys()
            .any(|key| !key.as_str().is_some_and(|k| fields.contains(&k)))
        {
            return Err("Unknown scenario field.");
        }
        if let Some(expected) = map.get(serde_yaml::Value::String("expected".into())) {
            let expected = expected.as_mapping().ok_or("Expected a matcher object.")?;
            let fields = [
                "exact",
                "contains",
                "not_contains",
                "regex",
                "tool_called",
                "cost_under",
            ];
            if expected
                .keys()
                .any(|key| !key.as_str().is_some_and(|k| fields.contains(&k)))
            {
                return Err("Unknown matcher field.");
            }
        }
        let case: Self = serde_yaml::from_value(value).map_err(|_| "Malformed scenario.")?;
        let s = &case.scenario;
        if s.id.trim().is_empty()
            || s.prompt.trim().is_empty()
            || s.follow_ups.iter().any(|s| s.trim().is_empty())
        {
            return Err("Scenario id and prompts must not be empty.");
        }
        if s.follow_ups.len() as u64 + 1 > u64::from(s.max_turns) || s.timeout_secs == 0 {
            return Err("Scenario turn or timeout limit is invalid.");
        }
        let valid_id = |id: &str| {
            !id.trim().is_empty() && id.chars().map(char::len_utf16).sum::<usize>() <= 128
        };
        let target_valid = match &case.target {
            Target::Bot { bot } => valid_id(bot),
            Target::Room { room_id, thread_id } => {
                valid_id(room_id) && thread_id.as_ref().is_none_or(|id| valid_id(id))
            }
        };
        if !target_valid
            || std::iter::once(&s.prompt)
                .chain(&s.follow_ups)
                .any(|p| p.chars().map(char::len_utf16).sum::<usize>() > 32_000)
        {
            return Err("Scenario destination or prompt is invalid.");
        }
        if s.expected
            .regex
            .as_ref()
            .is_some_and(|p| regex::Regex::new(p).is_err())
        {
            return Err("Invalid matcher regex.");
        }
        if s.expected
            .cost_under
            .is_some_and(|c| !c.is_finite() || c <= 0.0)
        {
            return Err("Invalid cost threshold.");
        }
        Ok(case)
    }
    /// Load one file or a directory in deterministic order. Validate the entire
    /// suite before dispatching anything; duplicate ids are rejected.
    pub fn load(path: &Path) -> Result<Vec<Self>, &'static str> {
        let mut paths = if path.is_dir() {
            std::fs::read_dir(path)
                .map_err(|_| "Cannot read scenario directory.")?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| "Cannot read scenario directory.")?
                .into_iter()
                .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("yaml" | "yml")))
                .collect()
        } else {
            vec![path.to_owned()]
        };
        paths.sort();
        let mut ids = HashSet::new();
        let mut cases = Vec::new();
        for path in paths {
            let text = std::fs::read_to_string(path).map_err(|_| "Cannot read scenario file.")?;
            let case = Self::from_yaml(&text)?;
            if !ids.insert(case.scenario.id.clone()) {
                return Err("Duplicate scenario id.");
            }
            cases.push(case);
        }
        if cases.is_empty() {
            return Err("No scenarios found.");
        }
        Ok(cases)
    }
}

/// Pure grading with optional evidence. Missing evidence never becomes a pass.
/// Known assertion failures take precedence and retain missing-evidence reasons.
pub fn grade_evidence(
    expected: &Expected,
    reply: &str,
    tokens: Option<u32>,
    cost: Option<f64>,
    tools: Option<&[String]>,
    max_tokens: u32,
) -> Outcome {
    let mut text = expected.clone();
    text.tool_called = None;
    text.cost_under = None;
    let mut failures = grade(&text, reply, None, None, &[], 0);
    let mut missing = Vec::new();
    if let Some(tool) = &expected.tool_called {
        match tools {
            Some(tools) if !tools.contains(tool) => {
                failures.push(format!("expected tool {tool:?} to be called"))
            }
            Some(_) => {}
            None => missing.push("Tool evidence unavailable from home.".into()),
        }
    }
    if let Some(limit) = expected.cost_under {
        match cost {
            Some(cost) if cost.is_finite() && cost >= 0.0 && cost < limit => {}
            Some(cost) if cost.is_finite() && cost >= 0.0 => {
                failures.push(format!("expected cost < {limit} but cost was {cost}"))
            }
            _ => missing.push("Cost evidence unavailable from home.".into()),
        }
    }
    if max_tokens > 0 {
        match tokens {
            Some(used) if used > max_tokens => {
                failures.push(format!("expected <= {max_tokens} tokens but used {used}"))
            }
            Some(_) => {}
            None => missing.push("Token evidence unavailable from home.".into()),
        }
    }
    if !failures.is_empty() {
        failures.extend(missing);
        Outcome::Fail { reasons: failures }
    } else if !missing.is_empty() {
        Outcome::Unavailable { reasons: missing }
    } else {
        Outcome::Pass
    }
}

/// Construct a result when a wait, dispatch, or interruption left missing evidence.
pub fn unavailable(case: &HomeScenario, reason: &str) -> ScenarioResult {
    ScenarioResult {
        id: case.scenario.id.clone(),
        description: case.scenario.description.clone(),
        outcome: Outcome::Unavailable {
            reasons: vec![reason.into()],
        },
        reply: String::new(),
        duration_ms: 0,
    }
}

/// Run turns serially under one deadline. The transcript remains usable if this
/// future is dropped: requests and admissions are synced before the next await.
pub async fn run_scenario(
    client: &HomeClient,
    case: &HomeScenario,
    transcript: &mut Transcript,
) -> io::Result<ScenarioResult> {
    let started = Instant::now();
    transcript.append(json!({"version":1,"kind":"scenario","id":case.scenario.id,"description":case.scenario.description}))?;
    let duration = Duration::from_secs(case.scenario.timeout_secs);
    let work = exchange(client, case, transcript);
    let (outcome, reply) = match tokio::time::timeout(duration, work).await {
        Ok(result) => result?,
        Err(_) => (Outcome::Unavailable { reasons: vec!["Deadline reached; home work was not cancelled. Resume admitted runs with wait.".into()] }, String::new()),
    };
    let result = ScenarioResult {
        id: case.scenario.id.clone(),
        description: case.scenario.description.clone(),
        outcome,
        reply,
        duration_ms: started.elapsed().as_millis(),
    };
    transcript.append(json!({"kind":"result","result":result}))?;
    Ok(result)
}

async fn exchange(
    client: &HomeClient,
    case: &HomeScenario,
    transcript: &mut Transcript,
) -> io::Result<(Outcome, String)> {
    let timeout = Duration::from_secs(case.scenario.timeout_secs);
    let mut thread = None;
    if let Target::Room { room_id, thread_id } = &case.target {
        let (rooms, exit) = execute_device(client, DeviceCommand::RoomsList, timeout).await;
        if exit != 0 {
            return Ok(missing("Room evidence unavailable."));
        }
        let resolved = rooms.data["rooms"]
            .as_array()
            .and_then(|rooms| rooms.iter().find(|r| r["id"] == *room_id))
            .and_then(|r| r["threadId"].as_str());
        thread = thread_id.clone().or_else(|| resolved.map(str::to_owned));
        if thread.is_none() {
            return Ok(missing("Room thread unavailable."));
        }
    }
    let mut final_reply = String::new();
    let mut bot_id: Option<String> = None;
    for (turn, prompt) in std::iter::once(&case.scenario.prompt)
        .chain(&case.scenario.follow_ups)
        .enumerate()
    {
        let nonce = fresh_client_nonce();
        transcript.append(json!({"kind":"request","turn":turn,"requestId":nonce,"prompt":prompt,"target":match &case.target { Target::Bot { bot } => json!({"kind":"bot","bot":bot}), Target::Room { room_id, .. } => json!({"kind":"room","roomId":room_id,"threadId":thread}) }}))?;
        let command = match &case.target {
            Target::Bot { bot } => DeviceCommand::Send {
                bot: bot_id.clone().unwrap_or_else(|| bot.clone()),
                text: prompt.clone(),
                request_id: nonce,
                wait: false,
            },
            Target::Room { room_id, .. } => DeviceCommand::RoomsSend {
                group_id: Some(room_id.clone()),
                room_name: None,
                thread_id: thread.clone(),
                text: prompt.clone(),
                client_nonce: nonce,
            },
        };
        let (sent, exit) = execute_device(client, command, timeout).await;
        // Even a failed/stopped admission can carry recovery ids.
        transcript.append(json!({"kind":"admission","turn":turn,"result":sent.json()}))?;
        if exit != 0 {
            return Ok(missing(
                sent.failure_reason
                    .as_deref()
                    .unwrap_or("Dispatch evidence unavailable."),
            ));
        }
        let task = sent.task_id.as_deref();
        let mut runs = sent.data["runIds"]
            .as_array()
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if runs.is_empty() {
            runs.extend(sent.run_id.iter().cloned());
        }
        if runs.is_empty() || task.is_none() {
            return Ok(missing(
                "No admitted run; room receipt is not an execution answer.",
            ));
        }
        if let Target::Bot { .. } = &case.target {
            let next_thread = sent.data["threadId"].as_str();
            if next_thread.is_none()
                || thread
                    .as_deref()
                    .is_some_and(|old| Some(old) != next_thread)
            {
                return Ok(missing("Conversation correlation changed."));
            }
            thread = next_thread.map(str::to_owned);
            bot_id = sent.bot.as_ref().map(|b| b.id.clone());
        }
        let mut replies = Vec::new();
        for run in runs {
            let (answer, exit) = execute_device(
                client,
                DeviceCommand::Wait {
                    run_id: run.clone(),
                },
                timeout,
            )
            .await;
            transcript.append(json!({"kind":"answer","turn":turn,"result":answer.json()}))?;
            if exit != 0 {
                return Ok(missing(
                    answer
                        .failure_reason
                        .as_deref()
                        .unwrap_or("Answer evidence unavailable."),
                ));
            }
            let detail = &answer.data["run"];
            if detail["threadId"].as_str() != thread.as_deref()
                || (sent.run_id.as_deref() == Some(&run) && detail["taskId"].as_str() != task)
                || bot_id
                    .as_deref()
                    .is_some_and(|id| detail["botId"].as_str() != Some(id))
            {
                return Ok(missing("Task or conversation correlation changed."));
            }
            replies.push(answer.reply_text);
        }
        final_reply = replies.join("\n");
    }
    // Reply text, tool-result blocks, and estimates are not cost/tool evidence.
    let outcome = grade_evidence(
        &case.scenario.expected,
        &final_reply,
        None,
        None,
        None,
        case.scenario.max_tokens,
    );
    Ok((outcome, final_reply))
}
fn missing(reason: &str) -> (Outcome, String) {
    (
        Outcome::Unavailable {
            reasons: vec![reason.into()],
        },
        String::new(),
    )
}
