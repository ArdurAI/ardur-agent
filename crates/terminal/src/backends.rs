//! Terminal backend implementations.

use crate::error::{Result, TerminalError};
use serde::{Deserialize, Serialize};

/// The kind of terminal backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BackendKind {
    Local,
    Docker,
    Ssh,
    Cloud,
}

/// A terminal backend that can execute commands.
pub trait TerminalBackend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn execute(&self, command: &str, timeout_secs: u64) -> Result<String>;
    fn execute_pty(&self, command: &str, timeout_secs: u64) -> Result<String>;
}

/// Local shell backend.
pub struct LocalBackend;

impl LocalBackend {
    pub fn new() -> Self {
        Self
    }
}

impl TerminalBackend for LocalBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Local
    }
    fn execute(&self, command: &str, _timeout_secs: u64) -> Result<String> {
        Ok(format!("[local] would execute: {command}"))
    }
    fn execute_pty(&self, command: &str, _timeout_secs: u64) -> Result<String> {
        Ok(format!("[local-pty] would execute: {command}"))
    }
}

/// Docker exec backend.
pub struct DockerBackend {
    container: String,
}

impl DockerBackend {
    pub fn new(container: impl Into<String>) -> Self {
        Self {
            container: container.into(),
        }
    }
}

impl TerminalBackend for DockerBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Docker
    }
    fn execute(&self, command: &str, _timeout_secs: u64) -> Result<String> {
        Ok(format!(
            "[docker:{}] would execute: {command}",
            self.container
        ))
    }
    fn execute_pty(&self, command: &str, _timeout_secs: u64) -> Result<String> {
        Ok(format!(
            "[docker-pty:{}] would execute: {command}",
            self.container
        ))
    }
}

/// SSH remote backend.
pub struct SshBackend {
    host: String,
    user: String,
}

impl SshBackend {
    pub fn new(host: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            user: user.into(),
        }
    }
}

impl TerminalBackend for SshBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Ssh
    }
    fn execute(&self, command: &str, _timeout_secs: u64) -> Result<String> {
        Ok(format!(
            "[ssh:{}@{}] would execute: {command}",
            self.user, self.host
        ))
    }
    fn execute_pty(&self, command: &str, _timeout_secs: u64) -> Result<String> {
        Ok(format!(
            "[ssh-pty:{}@{}] would execute: {command}",
            self.user, self.host
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_backend_execute() {
        let backend = LocalBackend::new();
        assert_eq!(backend.kind(), BackendKind::Local);
        let result = backend.execute("echo hello", 30).unwrap();
        assert!(result.contains("echo hello"));
    }

    #[test]
    fn docker_backend_execute() {
        let backend = DockerBackend::new("my-container");
        assert_eq!(backend.kind(), BackendKind::Docker);
        let result = backend.execute("ls", 30).unwrap();
        assert!(result.contains("my-container"));
    }

    #[test]
    fn ssh_backend_execute() {
        let backend = SshBackend::new("remote.host", "user");
        assert_eq!(backend.kind(), BackendKind::Ssh);
        let result = backend.execute("uptime", 30).unwrap();
        assert!(result.contains("remote.host"));
    }
}
