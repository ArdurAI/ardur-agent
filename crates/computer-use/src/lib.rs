#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! ardur-computer-use — macOS UI automation via Accessibility API.
//!
//! Plan family: §6.8 (`plans/6.8-computer-use-blueprint.md`).

mod error;
mod macos;
mod tools;

pub use error::{ComputerUseError, Result};
pub use macos::{MacOsAutomation, UiElement, UiAction};
pub use tools::{ComputerUseTool, ScreenshotTool};
