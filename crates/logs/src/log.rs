use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub level: LogLevel,
    pub component: String,
    pub message: String,
    pub metadata: HashMap<String, String>,
}

impl LogEntry {
    pub fn new(level: LogLevel, component: &str, message: &str) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            timestamp: Utc::now(),
            level,
            component: component.to_string(),
            message: message.to_string(),
            metadata: HashMap::new(),
        }
    }

    pub fn with_metadata(mut self, key: &str, value: &str) -> Self {
        self.metadata.insert(key.to_string(), value.to_string());
        self
    }
}

#[derive(Debug, Clone)]
pub struct LogStream {
    entries: std::sync::Arc<std::sync::RwLock<Vec<LogEntry>>>,
    max_size: usize,
}

impl Default for LogStream {
    fn default() -> Self {
        Self::new(10000)
    }
}

impl LogStream {
    pub fn new(max_size: usize) -> Self {
        Self {
            entries: std::sync::Arc::new(std::sync::RwLock::new(Vec::new())),
            max_size,
        }
    }

    pub fn append(&self, entry: LogEntry) -> crate::error::Result<()> {
        let mut entries = self.entries.write().map_err(|_| {
            crate::error::LogError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        if entries.len() >= self.max_size {
            entries.remove(0);
        }
        entries.push(entry);
        Ok(())
    }

    pub fn tail(&self, n: usize) -> crate::error::Result<Vec<LogEntry>> {
        let entries = self.entries.read().map_err(|_| {
            crate::error::LogError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let start = entries.len().saturating_sub(n);
        Ok(entries[start..].to_vec())
    }

    pub fn filter(
        &self,
        criteria: &crate::filter::FilterCriteria,
    ) -> crate::error::Result<Vec<LogEntry>> {
        let entries = self.entries.read().map_err(|_| {
            crate::error::LogError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(entries
            .iter()
            .filter(|e| criteria.matches(e))
            .cloned()
            .collect())
    }

    pub fn count(&self) -> crate::error::Result<usize> {
        let entries = self.entries.read().map_err(|_| {
            crate::error::LogError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(entries.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_entry_creation() {
        let entry = LogEntry::new(LogLevel::Info, "test", "Hello");
        assert_eq!(entry.level, LogLevel::Info);
        assert_eq!(entry.component, "test");
        assert_eq!(entry.message, "Hello");
    }

    #[test]
    fn test_log_stream_append() {
        let stream = LogStream::new(100);
        stream
            .append(LogEntry::new(LogLevel::Info, "a", "msg1"))
            .unwrap();
        stream
            .append(LogEntry::new(LogLevel::Info, "a", "msg2"))
            .unwrap();
        assert_eq!(stream.count().unwrap(), 2);
    }

    #[test]
    fn test_log_stream_tail() {
        let stream = LogStream::new(100);
        for i in 0..10 {
            stream
                .append(LogEntry::new(LogLevel::Info, "a", &format!("msg{}", i)))
                .unwrap();
        }
        let tail = stream.tail(3).unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].message, "msg7");
    }

    #[test]
    fn test_log_stream_max_size() {
        let stream = LogStream::new(5);
        for i in 0..10 {
            stream
                .append(LogEntry::new(LogLevel::Info, "a", &format!("msg{}", i)))
                .unwrap();
        }
        assert_eq!(stream.count().unwrap(), 5);
    }
}
