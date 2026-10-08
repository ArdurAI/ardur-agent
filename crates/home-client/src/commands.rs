use crate::{Error, HomeClient};
use home_protocol::{
    DeviceRunDetail, DispatchReceipt, DispatchState, FailureCategory, MessagePage, RunFailure,
    RunPage, RunStatus, string_len,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::{Instant, sleep, timeout};

pub const ANSWER_UNAVAILABLE: &str =
    "The task finished, but its answer is unavailable. Open it at home.";
pub const DEADLINE: &str = "The deadline was reached. Waiting stopped; the task was not cancelled.";
pub enum DeviceCommand {
    Send {
        bot: String,
        text: String,
        request_id: String,
        wait: bool,
    },
    Wait {
        run_id: String,
    },
    RunsList {
        cursor: Option<String>,
        limit: u32,
    },
    RunsShow {
        run_id: String,
    },
    TasksShow {
        task_id: String,
    },
    Stop {
        task_id: String,
    },
}
impl DeviceCommand {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Send { .. } => "send",
            Self::Wait { .. } => "wait",
            Self::RunsList { .. } => "runs list",
            Self::RunsShow { .. } => "runs show",
            Self::TasksShow { .. } => "tasks show",
            Self::Stop { .. } => "stop",
        }
    }
    fn waits(&self) -> bool {
        matches!(self, Self::Wait { .. } | Self::Send { wait: true, .. })
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandResult {
    version: u8,
    pub command: String,
    pub bot: Option<Bot>,
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    pub verdict: &'static str,
    pub reply_text: String,
    pub elapsed_ms: u64,
    pub failure_reason: Option<String>,
    pub data: Value,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Bot {
    pub id: String,
    pub name: String,
}
impl CommandResult {
    pub fn empty(command: &str) -> Self {
        Self {
            version: 1,
            command: command.into(),
            bot: None,
            run_id: None,
            task_id: None,
            verdict: "pass",
            reply_text: String::new(),
            elapsed_ms: 0,
            failure_reason: None,
            data: Value::Null,
        }
    }
    pub fn error(command: &str, error: Error) -> (Self, i32) {
        let mut result = Self::empty(command);
        result.verdict = "error";
        result.failure_reason = Some(error.to_string());
        (result, stage3_exit(error))
    }
    pub fn human(&self) -> String {
        if let Some(reason) = &self.failure_reason {
            return reason.clone();
        }
        if !self.reply_text.is_empty() {
            return self.reply_text.clone();
        }
        if self.command == "send" {
            return format!(
                "Task {}\nRun {}",
                self.task_id.as_deref().unwrap_or(""),
                self.run_id.as_deref().unwrap_or("")
            );
        }
        if self.command == "stop" {
            return format!(
                "Cancellation requested for {}.",
                self.task_id.as_deref().unwrap_or("")
            );
        }
        self.data.to_string()
    }
    pub fn json(&self) -> Value {
        redact(serde_json::to_value(self).expect("result fields serialize"))
    }
}
fn stage3_exit(error: Error) -> i32 {
    match error {
        Error::Input
        | Error::InvalidUnicode
        | Error::RequestChanged
        | Error::Storage
        | Error::Identity
        | Error::Access => 4,
        _ => 2,
    }
}
/// Redact each string before JSON encoding; never inspect escaped serialized prose.
fn redact(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(safe_output(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(redact).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, redact(v))).collect()),
        other => other,
    }
}
pub fn safe_output(s: &str) -> String {
    use std::sync::LazyLock;
    static KEYS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
        r"(?is)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|$)|\b(?:sk-[a-z0-9_-]{16,}|Bearer\s+[a-z0-9._~+/-]{16,})").expect("fixed redaction regex")
    });
    KEYS.replace_all(s, "[redacted]").into_owned()
}
pub async fn execute_device(
    client: &HomeClient,
    command: DeviceCommand,
    duration: Duration,
) -> (CommandResult, i32) {
    let started = Instant::now();
    let mut result = CommandResult::empty(command.name());
    let work = execute(client, &command, &mut result);
    let outcome = if command.waits() {
        match timeout(duration, work).await {
            Ok(outcome) => outcome,
            Err(_) => {
                result.verdict = "deadline";
                result.failure_reason = Some(DEADLINE.into());
                Ok(3)
            }
        }
    } else {
        work.await
    };
    let exit = match outcome {
        Ok(exit) => exit,
        Err(error) => {
            result.verdict = "error";
            result.failure_reason = Some(error.to_string());
            stage3_exit(error)
        }
    };
    result.elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    (result, exit)
}
fn input_id(id: &str) -> Result<(), Error> {
    if (1..=128).contains(&string_len(id)) {
        Ok(())
    } else {
        Err(Error::Input)
    }
}
fn project_run(value: Value) -> Result<DeviceRunDetail, Error> {
    let mut run: DeviceRunDetail = serde_json::from_value(value).map_err(|_| Error::Protocol)?;
    // Only fixed safe sentences enter output, even from an incompatible home.
    if let Some(failure) = &mut run.failure {
        failure.message = match run.status {
            RunStatus::Completed => ANSWER_UNAVAILABLE,
            RunStatus::Cancelled => "The bot run was cancelled.",
            _ => match failure.category {
                FailureCategory::SignedOut => {
                    "Sign in to the saved model connection at home, then try again."
                }
                FailureCategory::UsageLimit => {
                    "The saved model usage limit was reached; try again after it resets."
                }
                FailureCategory::ModelUnavailable => {
                    "The saved model is unavailable; check its pin at home."
                }
                _ => "The bot run failed; check its saved model and computer at home.",
            },
        }
        .into();
    }
    // Fail closed if an older home omits the new failure signal.
    if run.status == RunStatus::Completed && run.message_id.is_none() {
        run.failure = Some(RunFailure {
            category: FailureCategory::Other,
            message: ANSWER_UNAVAILABLE.into(),
        });
    }
    Ok(run)
}
async fn execute(
    client: &HomeClient,
    command: &DeviceCommand,
    result: &mut CommandResult,
) -> Result<i32, Error> {
    match command {
        DeviceCommand::RunsList { cursor, limit } => {
            if !(1..=100).contains(limit) {
                return Err(Error::Input);
            }
            let mut body = json!({"limit":limit});
            if let Some(cursor) = cursor {
                input_id(cursor)?;
                body["cursor"] = json!(cursor);
            }
            let page: RunPage = serde_json::from_value(client.request("runs/list", &body).await?)
                .map_err(|_| Error::Protocol)?;
            if page.runs.len() > *limit as usize {
                return Err(Error::Protocol);
            }
            let runs = page
                .runs
                .into_iter()
                .map(|r| project_run(json!(r)))
                .collect::<Result<Vec<_>, _>>()?;
            result.data = json!({"runs":runs,"nextCursor":page.next_cursor});
        }
        DeviceCommand::RunsShow { run_id } => {
            input_id(run_id)?;
            result.run_id = Some(run_id.clone());
            let value = client.request("runs/get", &json!({"runId":run_id})).await?;
            let run = project_run(value["run"].clone())?;
            if run.run_id != *run_id {
                return Err(Error::Protocol);
            }
            result.data = json!({"run":run});
        }
        DeviceCommand::TasksShow { task_id } => {
            input_id(task_id)?;
            result.task_id = Some(task_id.clone());
            let value = client
                .request("tasks/get", &json!({"taskId":task_id}))
                .await?;
            let run = project_run(value["task"].clone())?;
            if run.task_id != *task_id {
                return Err(Error::Protocol);
            }
            result.data = json!({"task":run});
        }
        DeviceCommand::Stop { task_id } => {
            input_id(task_id)?;
            result.task_id = Some(task_id.clone());
            let value = client.request("stop", &json!({"taskId":task_id})).await?;
            if value["cancelRequested"] != true {
                return Err(Error::Protocol);
            }
            result.data = json!({"cancelRequested":true});
        }
        DeviceCommand::Send {
            bot,
            text,
            request_id,
            wait,
        } => {
            input_id(bot)?;
            if !(16..=128).contains(&string_len(request_id))
                || text.trim().is_empty()
                || string_len(text) > 32_000
            {
                return Err(Error::Input);
            }
            let bots: Vec<Bot> = serde_json::from_value(
                client
                    .request("rpc", &json!({"procedure":"bots/list","input":{}}))
                    .await?,
            )
            .map_err(|_| Error::Protocol)?;
            let matches: Vec<Bot> = if let Some(exact) = bots.iter().find(|b| b.id == *bot) {
                vec![exact.clone()]
            } else {
                bots.into_iter().filter(|b| b.name == *bot).collect()
            };
            if matches.len() != 1 {
                return Err(Error::Input);
            }
            let bot = matches.into_iter().next().ok_or(Error::Input)?;
            let receipt: DispatchReceipt = serde_json::from_value(
                client
                    .request(
                        "dispatch",
                        &json!({"clientNonce":request_id,"botId":bot.id,"text":text}),
                    )
                    .await?,
            )
            .map_err(|_| Error::Protocol)?;
            if receipt.bot_id != bot.id {
                return Err(Error::Protocol);
            }
            result.bot = Some(bot);
            result.run_id = Some(receipt.run_id.clone());
            result.task_id = Some(receipt.task_id.clone());
            result.data = json!(receipt);
            if *wait {
                return wait_for_run(client, &receipt.run_id, Some(&receipt), result).await;
            }
            if matches!(
                receipt.state,
                DispatchState::Failed | DispatchState::Stopped
            ) {
                result.verdict = if receipt.state == DispatchState::Failed {
                    "failed"
                } else {
                    "stopped"
                };
                result.failure_reason = Some(
                    if receipt.state == DispatchState::Failed {
                        "The bot run failed; check it at home."
                    } else {
                        "The bot run stopped."
                    }
                    .into(),
                );
                return Ok(2);
            }
        }
        DeviceCommand::Wait { run_id } => {
            input_id(run_id)?;
            result.run_id = Some(run_id.clone());
            return wait_for_run(client, run_id, None, result).await;
        }
    }
    Ok(0)
}
async fn wait_for_run(
    client: &HomeClient,
    run_id: &str,
    receipt: Option<&DispatchReceipt>,
    result: &mut CommandResult,
) -> Result<i32, Error> {
    loop {
        let value = client.request("runs/get", &json!({"runId":run_id})).await?;
        let run = project_run(value["run"].clone())?;
        if run.run_id != run_id
            || receipt.is_some_and(|r| {
                r.task_id != run.task_id || r.bot_id != run.bot_id || r.thread_id != run.thread_id
            })
        {
            return Err(Error::Protocol);
        }
        result.task_id = Some(run.task_id.clone());
        result.data = json!({"run":run});
        if run.cancel_confirmed || matches!(run.status, RunStatus::Cancelled | RunStatus::Failed) {
            result.verdict = if run.cancel_confirmed || run.status == RunStatus::Cancelled {
                "stopped"
            } else {
                "failed"
            };
            result.failure_reason = Some(
                run.failure
                    .as_ref()
                    .map(|f| f.message.as_str())
                    .unwrap_or("The bot run stopped.")
                    .into(),
            );
            return Ok(2);
        }
        if matches!(
            run.status,
            RunStatus::WaitingInput | RunStatus::WaitingTakeover
        ) {
            result.verdict = "error";
            result.failure_reason = Some("This run needs input at home; waiting stopped.".into());
            return Ok(4);
        }
        if run.status == RunStatus::Completed {
            if let Some(failure) = run.failure {
                result.verdict = "error";
                result.failure_reason = Some(failure.message);
                return Ok(2);
            }
            let message_id = run.message_id.as_ref().ok_or(Error::Protocol)?;
            let page: MessagePage = serde_json::from_value(client.request("messages/get", &json!({"botId":run.bot_id,"threadId":run.thread_id,"around":{"messageId":message_id}})).await?).map_err(|_| Error::Protocol)?;
            let message = page.messages.iter().find(|m| {
                m.id == *message_id && m.run_id.as_deref() == Some(run_id) && m.role == "bot"
            });
            if page.thread_id != run.thread_id || page.messages.len() > 100 || message.is_none() {
                result.verdict = "error";
                result.failure_reason = Some(ANSWER_UNAVAILABLE.into());
                return Ok(2);
            }
            result.reply_text = message
                .ok_or(Error::Protocol)?
                .blocks
                .iter()
                .filter(|b| b.kind == "text" && !b.reasoning)
                .filter_map(|b| b.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n");
            if result.reply_text.is_empty() {
                result.verdict = "error";
                result.failure_reason = Some(ANSWER_UNAVAILABLE.into());
                return Ok(2);
            }
            return Ok(0);
        }
        // Bounded read-only polling. Cancellation requests alone never terminate a wait.
        sleep(Duration::from_secs(2)).await;
    }
}
