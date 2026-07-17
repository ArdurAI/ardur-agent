//! Identity newtypes. Kept local so the detector crate stands alone; the owning
//! runtime maps these to its own `SessionId` / run identity at the call site.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The session a detector state belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

/// A single agent run within a session — the unit a halt or kill tears down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub Uuid);

/// A monotonically increasing turn counter within a session. Sliding windows
/// are expressed in turns, so this is the detector's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub u64);

impl TurnId {
    /// The turn distance `self - earlier`, saturating at zero so a
    /// clock that never moves backward can compare windows without underflow.
    pub fn since(self, earlier: TurnId) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}
