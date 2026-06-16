use serde::{Deserialize, Serialize};

/// Wire-format adapter used by a hook entry or receipt annotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    /// Ardur's native Hermes-style hook format.
    Hermes,
    /// OpenClaw codex-compatible hook format.
    OpenClaw,
}

impl AdapterKind {
    /// Stable config/receipt string for this adapter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            AdapterKind::Hermes => "hermes",
            AdapterKind::OpenClaw => "openclaw",
        }
    }
}
