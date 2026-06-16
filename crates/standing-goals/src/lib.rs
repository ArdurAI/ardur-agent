pub mod error;
pub mod goal;
pub mod registry;

pub use error::{StandingGoalError, Result};
pub use goal::{StandingGoal, GoalId, GoalStatus, Frequency};
pub use registry::GoalRegistry;
