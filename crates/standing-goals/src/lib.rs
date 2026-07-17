pub mod error;
pub mod goal;
pub mod registry;

pub use error::{Result, StandingGoalError};
pub use goal::{Frequency, GoalId, GoalStatus, StandingGoal};
pub use registry::GoalRegistry;
