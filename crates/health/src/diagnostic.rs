use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub id: String,
    pub level: DiagnosticLevel,
    pub message: String,
    pub component: String,
    pub created_at: DateTime<Utc>,
    pub resolved: bool,
}

impl Diagnostic {
    pub fn new(id: &str, level: DiagnosticLevel, message: &str, component: &str) -> Self {
        Self {
            id: id.to_string(),
            level,
            message: message.to_string(),
            component: component.to_string(),
            created_at: Utc::now(),
            resolved: false,
        }
    }

    pub fn resolve(&mut self) {
        self.resolved = true;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticReport {
    pub diagnostics: Vec<Diagnostic>,
    pub generated_at: DateTime<Utc>,
}

impl DiagnosticReport {
    pub fn new() -> Self {
        Self {
            diagnostics: Vec::new(),
            generated_at: Utc::now(),
        }
    }

    pub fn add(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    pub fn unresolved(&self) -> Vec<&Diagnostic> {
        self.diagnostics.iter().filter(|d| !d.resolved).collect()
    }

    pub fn by_level(&self, level: DiagnosticLevel) -> Vec<&Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|d| d.level == level)
            .collect()
    }
}

impl Default for DiagnosticReport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diagnostic_creation() {
        let diag = Diagnostic::new("d1", DiagnosticLevel::Warning, "Low memory", "system");
        assert_eq!(diag.level, DiagnosticLevel::Warning);
        assert!(!diag.resolved);
    }

    #[test]
    fn test_diagnostic_resolve() {
        let mut diag = Diagnostic::new("d1", DiagnosticLevel::Warning, "Low memory", "system");
        diag.resolve();
        assert!(diag.resolved);
    }

    #[test]
    fn test_diagnostic_report() {
        let mut report = DiagnosticReport::new();
        report.add(Diagnostic::new(
            "d1",
            DiagnosticLevel::Info,
            "Info msg",
            "test",
        ));
        report.add(Diagnostic::new(
            "d2",
            DiagnosticLevel::Error,
            "Error msg",
            "test",
        ));
        assert_eq!(report.diagnostics.len(), 2);
        assert_eq!(report.unresolved().len(), 2);
        assert_eq!(report.by_level(DiagnosticLevel::Error).len(), 1);
    }
}
