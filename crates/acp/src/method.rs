//! ACP protocol and receipt vocabulary constants.

/// The current stable ACP major protocol version.
pub const ACP_PROTOCOL_VERSION: u16 = 1;

/// ACP JSON-RPC initialization method.
pub const ACP_METHOD_INITIALIZE: &str = "initialize";
/// ACP optional authentication method.
pub const ACP_METHOD_AUTHENTICATE: &str = "authenticate";
/// ACP method for creating a new session.
pub const ACP_METHOD_SESSION_NEW: &str = "session/new";
/// ACP method for loading an existing session.
pub const ACP_METHOD_SESSION_LOAD: &str = "session/load";
/// ACP method for prompting a session.
pub const ACP_METHOD_SESSION_PROMPT: &str = "session/prompt";
/// ACP method for canceling session work.
pub const ACP_METHOD_SESSION_CANCEL: &str = "session/cancel";
/// ACP method for changing a session mode.
pub const ACP_METHOD_SESSION_SET_MODE: &str = "session/set_mode";
/// ACP method for reading a text file through the client filesystem surface.
pub const ACP_METHOD_FS_READ_TEXT_FILE: &str = "fs/read_text_file";
/// ACP method for writing a text file through the client filesystem surface.
pub const ACP_METHOD_FS_WRITE_TEXT_FILE: &str = "fs/write_text_file";
/// ACP server notification for session updates.
pub const ACP_NOTIFICATION_SESSION_UPDATE: &str = "session/update";

/// §12.4 receipt verb for successful peer discovery.
pub const RECEIPT_ACP_PEER_DISCOVERED: &str = "acp.peer.discovered.v1";
/// §12.4 receipt verb for delegating a task to an external ACP peer.
pub const RECEIPT_ACP_TASK_DELEGATED_OUT: &str = "acp.task.delegated_out.v1";
/// §12.4 receipt verb for receiving a task from an external ACP peer.
pub const RECEIPT_ACP_TASK_RECEIVED_IN: &str = "acp.task.received_in.v1";
/// §12.4 receipt verb for refusing a peer or task by trust policy.
pub const RECEIPT_ACP_TRUST_REFUSED: &str = "acp.trust.refused.v1";
