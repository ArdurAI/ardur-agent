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
