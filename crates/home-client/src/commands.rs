use crate::redaction::redact;
use crate::storage::{FileStore, PendingRoomSend};
use crate::{Error, HomeClient, Refusal};
use home_protocol::{
    BoardSnapshot, ComputerEntry, DeviceRunDetail, DispatchReceipt, DispatchState, FailureCategory,
    MessagePage, RoomSummary, RunFailure, RunPage, RunStatus, ThreadSendResult, WorkItem,
    string_len,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::{Instant, sleep, timeout};

pub const ANSWER_UNAVAILABLE: &str =
    "The task finished, but its answer is unavailable. Open it at home.";
pub const DEADLINE: &str = "The deadline was reached. Waiting stopped; the task was not cancelled.";
#[derive(Clone)]
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
    ComputersList,
    BoardList {
        workspace: String,
        filter: Option<Value>,
        search: Option<String>,
    },
    BoardShow {
        workspace: String,
        item: String,
    },
    RoomsList,
    RoomsSend {
        group_id: Option<String>,
        room_name: Option<String>,
        thread_id: Option<String>,
        text: String,
        client_nonce: String,
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
            Self::ComputersList => "computers list",
            Self::BoardList { .. } => "board list",
            Self::BoardShow { .. } => "board show",
            Self::RoomsList => "rooms list",
            Self::RoomsSend { .. } => "rooms send",
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
        // Redact fields before either projection; serialized escapes never hide a credential.
        let value = self.json();
        if let Some(reason) = value["failureReason"].as_str() {
            return reason.into();
        }
        let reply = value["replyText"].as_str().unwrap_or("");
        if !reply.is_empty() {
            return reply.into();
        }
        let task = value["taskId"].as_str().unwrap_or("");
        let run = value["runId"].as_str().unwrap_or("");
        if self.command == "send" {
            return format!("Task {task}\nRun {run}");
        }
        if self.command == "stop" {
            return format!("Cancellation requested for {task}.");
        }
        if self.command == "computers list" {
            return value["data"]["computers"]
                .as_array()
                .map(|computers| {
                    computers
                        .iter()
                        .map(|c| {
                            format!(
                                "{}\t{}\t{}\t{}",
                                c["botId"].as_str().unwrap_or(""),
                                c["name"].as_str().unwrap_or(""),
                                c["status"]["kind"].as_str().unwrap_or(""),
                                c["status"]["state"].as_str().unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
        }
        if self.command == "board list" {
            return value["data"]["items"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|i| {
                            format!(
                                "{}\t{}\t{}",
                                i["id"].as_str().unwrap_or(""),
                                i["status"].as_str().unwrap_or(""),
                                i["title"].as_str().unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
        }
        if self.command == "board show" {
            let item = &value["data"]["item"];
            let mut lines = format!(
                "{}\t{}\tp{}\t{}",
                item["id"].as_str().unwrap_or(""),
                item["status"].as_str().unwrap_or(""),
                item["priority"].as_u64().map_or(0, |v| v.min(9)),
                item["title"].as_str().unwrap_or("")
            );
            for field in ["description", "acceptanceCriteria"] {
                let text = item[field].as_str().unwrap_or("");
                if !text.is_empty() {
                    lines.push('\n');
                    lines.push_str(text);
                }
            }
            return lines;
        }
        if self.command == "rooms list" {
            return value["data"]["rooms"]
                .as_array()
                .map(|rooms| {
                    rooms
                        .iter()
                        .map(|r| {
                            format!(
                                "{}\t{}{}",
                                r["id"].as_str().unwrap_or(""),
                                r["name"].as_str().unwrap_or(""),
                                if r["unread"] == true { " *" } else { "" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
        }
        if self.command == "rooms send" {
            let data = &value["data"];
            if data["kind"] == "receipt-only" {
                return data["receipt"]["text"].as_str().unwrap_or("").into();
            }
            let ids = data["runIds"].as_array().cloned().unwrap_or_default();
            let runs = ids
                .iter()
                .filter_map(|id| id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if ids.len() == 1 {
                return format!("Task {task}\nRun {runs}");
            }
            return format!("Task {task}\nRuns {runs}");
        }
        value["data"].to_string()
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
        DeviceCommand::ComputersList => {
            let computers: Vec<ComputerEntry> = serde_json::from_value(
                client
                    .request("rpc", &json!({"procedure":"computer/list","input":null}))
                    .await?,
            )
            .map_err(|_| Error::Protocol)?;
            result.data = json!({"computers":computers});
        }
        DeviceCommand::BoardList {
            workspace,
            filter,
            search,
        } => {
            input_id(workspace)?;
            let mut input = json!({"workspaceId":workspace});
            if let Some(filter) = filter {
                if !filter.is_object() {
                    return Err(Error::Input);
                }
                input["filter"] = filter.clone();
            }
            if let Some(search) = search {
                if string_len(search) > 500 {
                    return Err(Error::Input);
                }
                input["search"] = json!(search);
            }
            let value = match board_rpc(client, "board/snapshot", &input, result).await? {
                BoardOutcome::Value(value) => value,
                BoardOutcome::Refused => return Ok(4),
            };
            let snapshot: BoardSnapshot =
                serde_json::from_value(value).map_err(|_| Error::Protocol)?;
            result.data = json!({
                "workspace":workspace,
                "items":snapshot.items,
                "readyIds":snapshot.ready_ids,
                "blockedIds":snapshot.blocked_ids,
            });
        }
        DeviceCommand::BoardShow { workspace, item } => {
            input_id(workspace)?;
            input_id(item)?;
            let value = match board_rpc(
                client,
                "board/show",
                &json!({"workspaceId":workspace,"id":item}),
                result,
            )
            .await?
            {
                BoardOutcome::Value(value) => value,
                BoardOutcome::Refused => return Ok(4),
            };
            let item_value: WorkItem =
                serde_json::from_value(value).map_err(|_| Error::Protocol)?;
            if item_value.id != *item {
                return Err(Error::Protocol);
            }
            result.data = json!({"workspace":workspace,"item":item_value});
        }
        DeviceCommand::RoomsList => {
            let rooms: Vec<RoomSummary> =
                serde_json::from_value(client.request("rooms/list", &json!({})).await?)
                    .map_err(|_| Error::Protocol)?;
            result.data = json!({"rooms":rooms});
        }
        DeviceCommand::RoomsSend {
            group_id,
            room_name,
            thread_id,
            text,
            client_nonce,
        } => {
            validate_room_send(group_id, room_name, thread_id, text)?;
            if !(16..=128).contains(&string_len(client_nonce)) {
                return Err(Error::Input);
            }
            let mut body = json!({"clientNonce":client_nonce,"text":text});
            if let Some(group_id) = group_id {
                body["groupId"] = json!(group_id);
            } else {
                body["roomName"] = json!(room_name);
            }
            if let Some(thread_id) = thread_id {
                body["threadId"] = json!(thread_id);
            }
            let value = match client.request_answer("rooms/send", &body).await {
                Ok(value) => value,
                // Unknown or ambiguous room names keep the home's own answer.
                Err(Refusal::Answer {
                    status: 400,
                    message,
                    ..
                }) => {
                    result.verdict = "error";
                    result.failure_reason = Some(message);
                    return Ok(4);
                }
                Err(Refusal::Answer { .. }) => return Err(Error::Access),
                Err(Refusal::Failure(error)) => return Err(error),
            };
            let answer: ThreadSendResult =
                serde_json::from_value(value).map_err(|_| Error::Protocol)?;
            match answer {
                ThreadSendResult::Work {
                    task_id,
                    run_id,
                    seq,
                    run_ids,
                    receipt,
                } => {
                    let run_ids = run_ids
                        .filter(|ids| !ids.is_empty())
                        .unwrap_or_else(|| vec![run_id.clone()]);
                    result.task_id = Some(task_id.clone());
                    result.run_id = Some(run_id.clone());
                    result.data = json!({
                        "kind":"work",
                        "taskId":task_id,
                        "runId":run_id,
                        "runIds":run_ids,
                        "seq":seq,
                        "receipt":receipt,
                    });
                }
                ThreadSendResult::ReceiptOnly { seq, receipt } => {
                    // A greeting invents no task, run or wait; print its text honestly.
                    result.reply_text = receipt.text.clone();
                    result.data = json!({"kind":"receipt-only","seq":seq,"receipt":receipt});
                }
            }
        }
    }
    Ok(0)
}
fn validate_room_send(
    group_id: &Option<String>,
    room_name: &Option<String>,
    thread_id: &Option<String>,
    text: &str,
) -> Result<(), Error> {
    if group_id.is_some() == room_name.is_some() {
        return Err(Error::Input);
    }
    for id in [group_id, room_name, thread_id].into_iter().flatten() {
        input_id(id)?;
    }
    if text.trim().is_empty() || string_len(text) > 32_000 {
        return Err(Error::Input);
    }
    Ok(())
}
/// The home's shared board problem codes; only these carry the home's message.
fn is_board_problem_code(code: &str) -> bool {
    matches!(
        code,
        "not_installed"
            | "unsupported_version"
            | "no_board"
            | "busy"
            | "timeout"
            | "forbidden"
            | "access_lost"
            | "invalid_response"
            | "command_failed"
            | "item_not_found"
            | "created_incomplete"
            | "dolt_missing"
    )
}
enum BoardOutcome {
    Value(Value),
    /// The home's fixed answer is already stored in the result; exit 4.
    Refused,
}
/// Board reads keep the board's own access checks: a denial (403) or a known
/// board problem (400) answers with the home's fixed sentence; anything else
/// keeps the generic refusal mapping.
async fn board_rpc(
    client: &HomeClient,
    procedure: &str,
    input: &Value,
    result: &mut CommandResult,
) -> Result<BoardOutcome, Error> {
    match client
        .request_answer("rpc", &json!({"procedure":procedure,"input":input}))
        .await
    {
        Ok(value) => Ok(BoardOutcome::Value(value)),
        Err(Refusal::Answer {
            status,
            code,
            message,
        }) => {
            if code.as_deref().is_some_and(is_board_problem_code) {
                result.verdict = "error";
                result.failure_reason = Some(message);
                return Ok(BoardOutcome::Refused);
            }
            Err(if status == 403 {
                Error::Access
            } else {
                Error::Protocol
            })
        }
        Err(Refusal::Failure(error)) => Err(error),
    }
}
/// Fresh clientNonce for a new room send.
pub fn fresh_client_nonce() -> String {
    home_protocol::nonce()
}
/// Send a room message with durable lost-response recovery. A fresh clientNonce
/// is generated per new send and recorded beside the pairing; rerunning the
/// same room and exact text reuses it, so the home returns the original
/// admission instead of starting the message twice. A completed response
/// clears the record; storage failures only forfeit recovery.
pub async fn execute_room_send(
    store: &FileStore,
    client: &HomeClient,
    group_id: Option<String>,
    room_name: Option<String>,
    thread_id: Option<String>,
    text: String,
    duration: Duration,
) -> (CommandResult, i32) {
    if let Err(error) = validate_room_send(&group_id, &room_name, &thread_id, &text) {
        return CommandResult::error("rooms send", error);
    }
    let pending = store.load_room_send().ok().flatten();
    let reused = pending.as_ref().is_some_and(|p| {
        p.group_id == group_id
            && p.room_name == room_name
            && p.thread_id == thread_id
            && p.text == text
    });
    let client_nonce = if reused {
        pending
            .expect("reused implies present")
            .client_nonce
            .clone()
    } else {
        fresh_client_nonce()
    };
    let command = DeviceCommand::RoomsSend {
        group_id: group_id.clone(),
        room_name: room_name.clone(),
        thread_id: thread_id.clone(),
        text: text.clone(),
        client_nonce: client_nonce.clone(),
    };
    // Inputs above are valid, so any non-zero exit means a request may have
    // reached home: keep the recovery record unless a response completed.
    let (result, exit) = execute_device(client, command, duration).await;
    if exit == 0 {
        let _ = store.clear_room_send();
    } else {
        let record = PendingRoomSend {
            schema_version: 1,
            client_nonce,
            group_id,
            room_name,
            thread_id,
            text,
        };
        let _ = store.save_room_send(&record);
    }
    (result, exit)
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
