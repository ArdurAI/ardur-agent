use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Shared context passed to tasks during execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskContext {
    /// Arbitrary key-value data shared across tasks.
    pub data: HashMap<String, serde_json::Value>,
}

impl TaskContext {
    /// Create a new empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a value into the context.
    pub fn insert<K: Into<String>, V: Into<serde_json::Value>>(&mut self, key: K, value: V) {
        self.data.insert(key.into(), value.into());
    }

    /// Get a value from the context.
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.data.get(key)
    }
}
