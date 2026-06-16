pub mod error;
pub mod hook;

pub use error::{HookCompatError, Result};
pub use hook::{OpenClawHook, HookRegistry};
