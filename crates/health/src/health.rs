use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    pub name: String,
    pub status: HealthStatus,
    pub message: String,
    pub checked_at: DateTime<Utc>,
    pub latency_ms: u64,
    pub metadata: HashMap<String, String>,
}

impl HealthCheck {
    pub fn new(name: &str, status: HealthStatus) -> Self {
        Self {
            name: name.to_string(),
            status,
            message: String::new(),
            checked_at: Utc::now(),
            latency_ms: 0,
            metadata: HashMap::new(),
        }
    }

    pub fn with_message(mut self, msg: &str) -> Self {
        self.message = msg.to_string();
        self
    }

    pub fn with_latency(mut self, ms: u64) -> Self {
        self.latency_ms = ms;
        self
    }
}

#[derive(Debug, Clone)]
pub struct HealthMonitor {
    checks: std::sync::Arc<std::sync::RwLock<HashMap<String, HealthCheck>>>,
}

impl Default for HealthMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthMonitor {
    pub fn new() -> Self {
        Self {
            checks: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    pub fn record(&self, check: HealthCheck) -> crate::error::Result<()> {
        let mut checks = self
            .checks
            .write()
            .map_err(|_| crate::error::HealthError::Io(std::io::Error::other("poisoned lock")))?;
        checks.insert(check.name.clone(), check);
        Ok(())
    }

    pub fn get(&self, name: &str) -> crate::error::Result<HealthCheck> {
        let checks = self
            .checks
            .read()
            .map_err(|_| crate::error::HealthError::Io(std::io::Error::other("poisoned lock")))?;
        checks
            .get(name)
            .cloned()
            .ok_or_else(|| crate::error::HealthError::ComponentNotFound(name.to_string()))
    }

    pub fn overall(&self) -> crate::error::Result<HealthStatus> {
        let checks = self
            .checks
            .read()
            .map_err(|_| crate::error::HealthError::Io(std::io::Error::other("poisoned lock")))?;
        if checks.is_empty() {
            return Ok(HealthStatus::Unknown);
        }
        let mut has_degraded = false;
        for check in checks.values() {
            match check.status {
                HealthStatus::Unhealthy => return Ok(HealthStatus::Unhealthy),
                HealthStatus::Degraded => has_degraded = true,
                _ => {}
            }
        }
        if has_degraded {
            Ok(HealthStatus::Degraded)
        } else {
            Ok(HealthStatus::Healthy)
        }
    }

    pub fn list(&self) -> crate::error::Result<Vec<HealthCheck>> {
        let checks = self
            .checks
            .read()
            .map_err(|_| crate::error::HealthError::Io(std::io::Error::other("poisoned lock")))?;
        Ok(checks.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_check_creation() {
        let check = HealthCheck::new("db", HealthStatus::Healthy).with_message("connected");
        assert_eq!(check.name, "db");
        assert_eq!(check.status, HealthStatus::Healthy);
        assert_eq!(check.message, "connected");
    }

    #[test]
    fn test_monitor_overall_healthy() {
        let monitor = HealthMonitor::new();
        monitor
            .record(HealthCheck::new("a", HealthStatus::Healthy))
            .unwrap();
        monitor
            .record(HealthCheck::new("b", HealthStatus::Healthy))
            .unwrap();
        assert_eq!(monitor.overall().unwrap(), HealthStatus::Healthy);
    }

    #[test]
    fn test_monitor_overall_degraded() {
        let monitor = HealthMonitor::new();
        monitor
            .record(HealthCheck::new("a", HealthStatus::Healthy))
            .unwrap();
        monitor
            .record(HealthCheck::new("b", HealthStatus::Degraded))
            .unwrap();
        assert_eq!(monitor.overall().unwrap(), HealthStatus::Degraded);
    }

    #[test]
    fn test_monitor_overall_unhealthy() {
        let monitor = HealthMonitor::new();
        monitor
            .record(HealthCheck::new("a", HealthStatus::Healthy))
            .unwrap();
        monitor
            .record(HealthCheck::new("b", HealthStatus::Unhealthy))
            .unwrap();
        assert_eq!(monitor.overall().unwrap(), HealthStatus::Unhealthy);
    }

    #[test]
    fn test_monitor_get() {
        let monitor = HealthMonitor::new();
        monitor
            .record(HealthCheck::new("db", HealthStatus::Healthy))
            .unwrap();
        let check = monitor.get("db").unwrap();
        assert_eq!(check.name, "db");
    }

    #[test]
    fn test_monitor_get_missing() {
        let monitor = HealthMonitor::new();
        assert!(monitor.get("missing").is_err());
    }
}
