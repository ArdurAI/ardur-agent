//! Stage 3 wire records; unknown remote fields never enter command output.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DispatchState {
    WaitingForHome,
    Accepted,
    Running,
    Done,
    Failed,
    Stopped,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Leased,
    Running,
    WaitingInput,
    WaitingTakeover,
    Completed,
    Failed,
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureCategory {
    UsageLimit,
    SignedOut,
    MaxTurns,
    ModelUnavailable,
    ConfigurationInvalid,
    ConnectionMissing,
    ExperimentalOff,
    ComputerUnsupported,
    DestinationsBot,
    DestinationsSpace,
    Stopped,
    SessionStartFailed,
    ModelContextTooSmall,
    RuntimeToolCatalogMismatch,
    RuntimeProfileUnacknowledged,
    ProviderRequestTooLarge,
    ProviderResponseTooLarge,
    ProviderGrantRefused,
    ProviderAuthFailed,
    ProviderRequestFailed,
    Other,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct RunFailure {
    pub category: FailureCategory,
    pub message: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchReceipt {
    pub task_id: String,
    pub run_id: String,
    pub thread_id: String,
    pub bot_id: String,
    pub state: DispatchState,
    pub cancel_requested: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRunDetail {
    pub task_id: String,
    pub run_id: String,
    pub thread_id: String,
    pub bot_id: String,
    pub state: DispatchState,
    pub cancel_requested: bool,
    pub status: RunStatus,
    pub cancel_confirmed: bool,
    pub message_id: Option<String>,
    pub failure: Option<RunFailure>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}
#[derive(Deserialize)]
pub struct RunPage {
    pub runs: Vec<DeviceRunDetail>,
    #[serde(rename = "nextCursor")]
    pub next_cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    pub thread_id: String,
    pub messages: Vec<Message>,
    pub older_cursor: Option<u64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub run_id: Option<String>,
    pub role: String,
    pub blocks: Vec<MessageBlock>,
}
#[derive(Deserialize)]
pub struct MessageBlock {
    pub kind: String,
    pub text: Option<String>,
    #[serde(default)]
    pub reasoning: bool,
}

/// Stage 4 daily reads and room sends; comments, history and other bulky
/// remote fields never enter command output.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerStatus {
    pub computer_id: String,
    pub bot_id: String,
    pub mode: String,
    pub kind: String,
    pub state: String,
    pub control_holder: String,
    pub control_bot_id: Option<String>,
    pub takeover_requested: bool,
    pub screen_available: bool,
    pub screen_width: u64,
    pub screen_height: u64,
    pub home_revision: Option<String>,
    pub busy_bot_name: Option<String>,
    pub can_update: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerEntry {
    pub bot_id: String,
    pub name: String,
    pub status: ComputerStatus,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardDependency {
    pub id: String,
    #[serde(rename = "type")]
    pub dependency_type: String,
    pub direction: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkItem {
    pub id: String,
    pub title: String,
    pub description: String,
    pub acceptance_criteria: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub status: String,
    pub priority: u8,
    pub assignee: Option<String>,
    pub labels: Vec<String>,
    pub parent: Option<String>,
    pub dependencies: Vec<BoardDependency>,
    pub due_at: Option<String>,
    pub defer_until: Option<String>,
    pub estimate_minutes: Option<u64>,
    pub external_ref: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub comment_count: u64,
    pub close_when_done: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardSnapshot {
    pub items: Vec<WorkItem>,
    pub ready_ids: Vec<String>,
    pub blocked_ids: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomMember {
    pub bot_id: String,
    pub name: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomSummary {
    pub id: String,
    pub name: String,
    pub pinned: bool,
    pub thread_id: String,
    pub preview: String,
    pub unread: bool,
    pub members: Vec<RoomMember>,
    pub updated_at: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChiefReceipt {
    pub id: String,
    pub thread_id: String,
    pub seq: u64,
    pub bot_id: String,
    pub request_message_id: String,
    pub key: String,
    pub text: String,
    pub created_at: String,
}
/// Room send answers: work keeps every run id; a greeting invents no run.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ThreadSendResult {
    #[serde(rename_all = "camelCase")]
    Work {
        task_id: String,
        run_id: String,
        seq: u64,
        #[serde(default)]
        run_ids: Option<Vec<String>>,
        #[serde(default)]
        receipt: Option<ChiefReceipt>,
    },
    #[serde(rename_all = "camelCase")]
    ReceiptOnly { seq: u64, receipt: ChiefReceipt },
}
