//! §1.9 — a minimal, real "agent background task" runtime: `/background`
//! (aliases `/bg`, `/btw`) spawns a prompt onto its own `tokio::spawn`ed
//! future that runs concurrently with the foreground REPL loop, tracked in
//! an in-memory [`TaskRegistry`] `/tasks`/`/task status`/`/task result`/
//! `/task cancel` read and steer.
//!
//! Scoped to the blueprint's "Recommended MVP Cut": a task ledger, the
//! `/background`/`/bg`/`/btw`/`/tasks`/`/task status`/`/task result`/
//! `/task cancel` command surface, and the agent (prompt-through-a-model)
//! task type. Process background tasks (an external process under Ardur
//! control), task flows (multiple coordinated child tasks), scheduled
//! background tasks (cron convergence), and durable cross-restart
//! persistence are all explicitly deferred — a task's record lives only for
//! the `ardur chat` process's lifetime, same as the rest of this process's
//! in-memory REPL state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ardur_runtime::{ReceiptId, SessionId};

use crate::error::CliError;
use crate::fused::FusedEngine;

/// Stable, time-ordered identifier of a background task (UUIDv7, matching
/// [`ardur_runtime::SessionId`]/`TurnId`'s ID strategy).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TaskId(pub uuid::Uuid);

impl TaskId {
    /// Mint a fresh, time-ordered task id.
    #[must_use]
    fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A background task's lifecycle state.
///
/// A reduced slice of the blueprint's full status model
/// (`queued/starting/running/waiting_for_input/blocked_on_approval/paused/
/// completed/failed/cancelled/timed_out/lost/archived`) — this MVP's agent
/// background runtime has no approval gates, no distinct timeout state (a
/// timeout would surface as `Failed`, though timeout detection itself isn't
/// wired up yet either), and no pause/resume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    /// Recorded, not yet started.
    Queued,
    /// Actively running.
    Running,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
    /// Cancelled by explicit user action.
    Cancelled,
}

impl TaskStatus {
    /// Whether this status can still transition (i.e. is not terminal).
    /// Matches the blueprint's status-model rule: "terminal states do not
    /// return to running".
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, TaskStatus::Queued | TaskStatus::Running)
    }

    #[must_use]
    fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One background task's durable-for-the-process-lifetime record.
#[derive(Clone, Debug)]
pub struct BackgroundTask {
    /// This task's stable id.
    pub id: TaskId,
    /// The session that started it (invariant 4: "a background task has an
    /// owner session and an owner project" — project scoping isn't modeled
    /// yet, session is).
    pub owner_session_id: SessionId,
    /// The prompt it's running.
    pub prompt: String,
    /// Current lifecycle state.
    pub status: TaskStatus,
    /// The result, once `Completed`.
    pub result: Option<String>,
    /// The failure message, once `Failed`.
    pub error: Option<String>,
    /// The terminal receipt (completed/failed/cancelled), once minted.
    pub receipt_id: Option<ReceiptId>,
    /// **§1.10.** Steering directives accepted against this task, in
    /// request order. Durably receipted evidence that a steering request
    /// was made and accepted — see
    /// [`ardur_fused_runtime::FusedRuntime::accept_steer_directive`] for why
    /// they don't yet change this MVP's one-shot task's in-flight behavior.
    pub steer_directives: Vec<SteerDirective>,
}

/// **§1.10.** One steering directive accepted against a task.
#[derive(Clone, Debug)]
pub struct SteerDirective {
    /// The steering message.
    pub message: String,
    /// The receipt minted for accepting it.
    pub receipt_id: ReceiptId,
}

impl BackgroundTask {
    fn new(id: TaskId, owner_session_id: SessionId, prompt: String) -> Self {
        Self {
            id,
            owner_session_id,
            prompt,
            status: TaskStatus::Queued,
            result: None,
            error: None,
            receipt_id: None,
            steer_directives: Vec::new(),
        }
    }

    fn start(&mut self) {
        self.status = TaskStatus::Running;
    }

    fn complete(&mut self, result: String, receipt_id: ReceiptId) {
        self.status = TaskStatus::Completed;
        self.result = Some(result);
        self.receipt_id = Some(receipt_id);
    }

    fn fail(&mut self, error: String, receipt_id: Option<ReceiptId>) {
        self.status = TaskStatus::Failed;
        self.error = Some(error);
        self.receipt_id = receipt_id;
    }

    fn cancel(&mut self, receipt_id: ReceiptId) {
        self.status = TaskStatus::Cancelled;
        self.receipt_id = Some(receipt_id);
    }

    fn steer(&mut self, message: String, receipt_id: ReceiptId) {
        self.steer_directives.push(SteerDirective {
            message,
            receipt_id,
        });
    }
}

/// One entry the registry tracks: the shared, lock-guarded record every
/// reader (`/tasks`, `/task status`) sees a live snapshot of, and the
/// `JoinHandle` `/task cancel` aborts.
struct TaskEntry {
    record: Arc<Mutex<BackgroundTask>>,
    join: tokio::task::JoinHandle<()>,
}

/// The in-memory ledger of every background task started this process.
///
/// Owned once by the REPL loop (alongside `history`/`bus`/`state`) and
/// threaded by reference into command dispatch — the same shape
/// `InMemoryCommandBus` already has.
#[derive(Default)]
pub struct TaskRegistry {
    tasks: Mutex<HashMap<TaskId, TaskEntry>>,
}

impl TaskRegistry {
    /// A fresh, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// **§1.9.** Spawn `prompt` as a new background task and return its id
    /// immediately — the task runs concurrently, tracked in the registry.
    pub fn spawn(&self, engine: Arc<FusedEngine>, prompt: String) -> TaskId {
        let id = TaskId::new();
        let record = Arc::new(Mutex::new(BackgroundTask::new(
            id,
            engine.session_id(),
            prompt.clone(),
        )));
        let record_for_task = Arc::clone(&record);
        let join = tokio::spawn(async move {
            record_for_task
                .lock()
                .expect("background task record mutex")
                .start();
            match engine.run_background_task(&prompt).await {
                Ok(outcome) => {
                    let mut task = record_for_task
                        .lock()
                        .expect("background task record mutex");
                    match (outcome.result, outcome.error) {
                        (Some(result), _) => task.complete(result, outcome.receipt_id),
                        (None, error) => task.fail(
                            error.unwrap_or_else(|| "task failed with no error detail".to_string()),
                            Some(outcome.receipt_id),
                        ),
                    }
                }
                Err(e) => {
                    record_for_task
                        .lock()
                        .expect("background task record mutex")
                        .fail(e.to_string(), None);
                }
            }
        });
        self.tasks
            .lock()
            .expect("task registry mutex")
            .insert(id, TaskEntry { record, join });
        id
    }

    /// Every task started this process, in no particular guaranteed order.
    #[must_use]
    pub fn list(&self) -> Vec<BackgroundTask> {
        self.tasks
            .lock()
            .expect("task registry mutex")
            .values()
            .map(|entry| {
                entry
                    .record
                    .lock()
                    .expect("background task record mutex")
                    .clone()
            })
            .collect()
    }

    /// One task's current record, if it exists.
    #[must_use]
    pub fn get(&self, id: TaskId) -> Option<BackgroundTask> {
        self.tasks
            .lock()
            .expect("task registry mutex")
            .get(&id)
            .map(|entry| {
                entry
                    .record
                    .lock()
                    .expect("background task record mutex")
                    .clone()
            })
    }

    /// **§1.9.** Cancel an active (queued or running) task: abort its
    /// spawned future and mint a `task.background.cancelled.v1` receipt.
    /// Rejects a task id that doesn't exist or is already terminal — "rerun
    /// creates a new task", per the blueprint's status-transition rules, so
    /// a terminal task cannot be un-cancelled or re-cancelled.
    ///
    /// # Errors
    /// Returns [`CliError`] if `id` is unknown, the task is already
    /// terminal, or minting the cancellation receipt fails.
    pub async fn cancel(&self, engine: &FusedEngine, id: TaskId) -> Result<(), CliError> {
        {
            let tasks = self.tasks.lock().expect("task registry mutex");
            let entry = tasks
                .get(&id)
                .ok_or_else(|| CliError::State(format!("task {id} not found")))?;
            let active = entry
                .record
                .lock()
                .expect("background task record mutex")
                .status
                .is_active();
            if !active {
                return Err(CliError::State(format!("task {id} is already terminal")));
            }
        }
        // Mint the cancellation receipt before aborting: the receipt records
        // that the *user* cancelled the task, not that it silently vanished.
        let receipt_id = engine.cancel_background_task().await?;
        let tasks = self.tasks.lock().expect("task registry mutex");
        let entry = tasks
            .get(&id)
            .ok_or_else(|| CliError::State(format!("task {id} not found")))?;
        entry.join.abort();
        entry
            .record
            .lock()
            .expect("background task record mutex")
            .cancel(receipt_id);
        Ok(())
    }

    /// **§1.10.** Accept a steering directive against an active task: mints
    /// `input.steer.accepted.v1` and durably records it on the task. See
    /// [`ardur_fused_runtime::FusedRuntime::accept_steer_directive`] for the
    /// documented limitation that this doesn't yet change the target's
    /// in-flight behavior.
    ///
    /// # Errors
    /// Returns [`CliError`] if `id` is unknown, the task is already
    /// terminal, or minting the receipt fails.
    pub async fn steer(
        &self,
        engine: &FusedEngine,
        id: TaskId,
        message: String,
    ) -> Result<(), CliError> {
        {
            let tasks = self.tasks.lock().expect("task registry mutex");
            let entry = tasks
                .get(&id)
                .ok_or_else(|| CliError::State(format!("task {id} not found")))?;
            let active = entry
                .record
                .lock()
                .expect("background task record mutex")
                .status
                .is_active();
            if !active {
                return Err(CliError::State(format!(
                    "task {id} is already terminal; steering has nothing to reach"
                )));
            }
        }
        let receipt_id = engine.accept_steer_directive(id.0, &message).await?;
        let tasks = self.tasks.lock().expect("task registry mutex");
        let entry = tasks
            .get(&id)
            .ok_or_else(|| CliError::State(format!("task {id} not found")))?;
        entry
            .record
            .lock()
            .expect("background task record mutex")
            .steer(message, receipt_id);
        Ok(())
    }

    /// **§1.10.** Interrupt an active task: mint `input.interrupt.accepted.v1`,
    /// abort its spawned future, and mark it cancelled — mechanically the
    /// same effect as [`cancel`](Self::cancel), but a distinct receipted
    /// intent (see [`ardur_fused_runtime::FusedRuntime::accept_interrupt`]).
    ///
    /// # Errors
    /// Returns [`CliError`] if `id` is unknown, the task is already
    /// terminal, or minting the receipt fails.
    pub async fn interrupt(&self, engine: &FusedEngine, id: TaskId) -> Result<(), CliError> {
        {
            let tasks = self.tasks.lock().expect("task registry mutex");
            let entry = tasks
                .get(&id)
                .ok_or_else(|| CliError::State(format!("task {id} not found")))?;
            let active = entry
                .record
                .lock()
                .expect("background task record mutex")
                .status
                .is_active();
            if !active {
                return Err(CliError::State(format!("task {id} is already terminal")));
            }
        }
        let receipt_id = engine.accept_interrupt(id.0).await?;
        let tasks = self.tasks.lock().expect("task registry mutex");
        let entry = tasks
            .get(&id)
            .ok_or_else(|| CliError::State(format!("task {id} not found")))?;
        entry.join.abort();
        entry
            .record
            .lock()
            .expect("background task record mutex")
            .cancel(receipt_id);
        Ok(())
    }

    /// **§1.10.** `/queue`'s summary: how many tasks are active, how many
    /// are terminal, and how many steering directives are pending delivery
    /// (accepted but, per this MVP's one-shot task runtime, not yet
    /// deliverable — see [`steer`](Self::steer)).
    #[must_use]
    pub fn queue_summary(&self) -> QueueSummary {
        let all = self.list();
        let active = all.iter().filter(|t| t.status.is_active()).count();
        let terminal = all.len() - active;
        let pending_directives = all.iter().map(|t| t.steer_directives.len()).sum();
        QueueSummary {
            active_tasks: active,
            terminal_tasks: terminal,
            pending_steer_directives: pending_directives,
        }
    }
}

/// **§1.10.** `/queue` and `/status`'s task-queue summary.
#[derive(Clone, Copy, Debug)]
pub struct QueueSummary {
    /// Tasks currently queued or running.
    pub active_tasks: usize,
    /// Tasks that have reached a terminal state.
    pub terminal_tasks: usize,
    /// Steering directives accepted but not yet deliverable (see
    /// [`TaskRegistry::steer`]'s docs on this MVP's limitation).
    pub pending_steer_directives: usize,
}
