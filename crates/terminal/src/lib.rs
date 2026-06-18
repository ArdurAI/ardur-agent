#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! ardur-terminal — PTY-style terminal backends: local, Docker exec, SSH remote.
//!
//! Plan family: §6.5 (`plans/6.5-terminal-backends-blueprint.md`).

mod backends;
mod error;
mod tools;

pub use backends::{
    BackendKind, DockerBackend, ExecResult, LocalBackend, ModalBackend, SshBackend,
    TerminalBackend, TerminalPolicy,
};
pub use error::{Result, TerminalError};
pub use tools::{TerminalExecTool, TerminalSessionTool};
