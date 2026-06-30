use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterCriteria {
    pub level: Option<crate::log::LogLevel>,
    pub component: Option<String>,
    pub message_contains: Option<String>,
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub until: Option<chrono::DateTime<chrono::Utc>>,
}

impl FilterCriteria {
    pub fn new() -> Self {
        Self {
            level: None,
            component: None,
            message_contains: None,
            since: None,
            until: None,
        }
    }

    pub fn with_level(mut self, level: crate::log::LogLevel) -> Self {
        self.level = Some(level);
        self
    }

    pub fn with_component(mut self, component: &str) -> Self {
        self.component = Some(component.to_string());
        self
    }

    pub fn with_message_contains(mut self, text: &str) -> Self {
        self.message_contains = Some(text.to_string());
        self
    }

    pub fn matches(&self, entry: &crate::log::LogEntry) -> bool {
        if let Some(level) = &self.level {
            if entry.level != *level {
                return false;
            }
        }
        if let Some(component) = &self.component {
            if entry.component != *component {
                return false;
            }
        }
        if let Some(text) = &self.message_contains {
            if !entry.message.contains(text) {
                return false;
            }
        }
        if let Some(since) = &self.since {
            if entry.timestamp < *since {
                return false;
            }
        }
        if let Some(until) = &self.until {
            if entry.timestamp > *until {
                return false;
            }
        }
        true
    }
}

impl Default for FilterCriteria {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct LogFilter {
    criteria: FilterCriteria,
}

impl LogFilter {
    pub fn new(criteria: FilterCriteria) -> Self {
        Self { criteria }
    }

    pub fn apply(&self, entries: &[crate::log::LogEntry]) -> Vec<crate::log::LogEntry> {
        entries
            .iter()
            .filter(|e| self.criteria.matches(e))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_criteria_level() {
        let criteria = FilterCriteria::new().with_level(crate::log::LogLevel::Error);
        let entry = crate::log::LogEntry::new(crate::log::LogLevel::Error, "test", "err");
        assert!(criteria.matches(&entry));
        let entry2 = crate::log::LogEntry::new(crate::log::LogLevel::Info, "test", "info");
        assert!(!criteria.matches(&entry2));
    }

    #[test]
    fn test_filter_criteria_component() {
        let criteria = FilterCriteria::new().with_component("db");
        let entry = crate::log::LogEntry::new(crate::log::LogLevel::Info, "db", "msg");
        assert!(criteria.matches(&entry));
        let entry2 = crate::log::LogEntry::new(crate::log::LogLevel::Info, "api", "msg");
        assert!(!criteria.matches(&entry2));
    }

    #[test]
    fn test_filter_criteria_message() {
        let criteria = FilterCriteria::new().with_message_contains("error");
        let entry =
            crate::log::LogEntry::new(crate::log::LogLevel::Info, "test", "an error occurred");
        assert!(criteria.matches(&entry));
        let entry2 = crate::log::LogEntry::new(crate::log::LogLevel::Info, "test", "success");
        assert!(!criteria.matches(&entry2));
    }

    #[test]
    fn test_log_filter_apply() {
        let criteria = FilterCriteria::new().with_level(crate::log::LogLevel::Error);
        let filter = LogFilter::new(criteria);
        let entries = vec![
            crate::log::LogEntry::new(crate::log::LogLevel::Info, "a", "info"),
            crate::log::LogEntry::new(crate::log::LogLevel::Error, "a", "err"),
            crate::log::LogEntry::new(crate::log::LogLevel::Debug, "a", "dbg"),
        ];
        let filtered = filter.apply(&entries);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].message, "err");
    }
}
