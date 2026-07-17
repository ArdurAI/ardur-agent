//! Report rendering — turn a slice of [`ScenarioResult`] into one of three
//! output formats: machine-readable JSON, CI-friendly JUnit XML, or a
//! human-friendly Markdown summary table.

use crate::runner::{Outcome, ScenarioResult};

/// The selectable output format for `ardur-eval run --output <fmt>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Full detail, machine-readable.
    Json,
    /// JUnit XML for CI test reporters.
    Junit,
    /// Markdown summary table for humans.
    Markdown,
}

impl std::str::FromStr for Format {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "json" => Ok(Format::Json),
            "junit" | "xml" => Ok(Format::Junit),
            "markdown" | "md" => Ok(Format::Markdown),
            other => Err(format!(
                "unknown output format {other:?} (expected json|junit|markdown)"
            )),
        }
    }
}

/// Aggregate pass/fail/error counts over a result set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// Number of scenarios that passed.
    pub passed: usize,
    /// Number of scenarios that failed a matcher.
    pub failed: usize,
    /// Number of scenarios that errored (transport/timeout/etc.).
    pub errored: usize,
}

impl Summary {
    /// Tally a result slice.
    pub fn of(results: &[ScenarioResult]) -> Self {
        let mut s = Summary::default();
        for r in results {
            match &r.outcome {
                Outcome::Pass => s.passed += 1,
                Outcome::Fail { .. } => s.failed += 1,
                Outcome::Error { .. } => s.errored += 1,
            }
        }
        s
    }

    /// Total scenarios tallied.
    pub fn total(&self) -> usize {
        self.passed + self.failed + self.errored
    }

    /// True when no scenario failed or errored — the run is green.
    pub fn is_green(&self) -> bool {
        self.failed == 0 && self.errored == 0
    }
}

/// Render `results` in the requested `format`.
pub fn render(results: &[ScenarioResult], format: Format) -> String {
    match format {
        Format::Json => render_json(results),
        Format::Junit => render_junit(results),
        Format::Markdown => render_markdown(results),
    }
}

fn render_json(results: &[ScenarioResult]) -> String {
    let summary = Summary::of(results);
    let report = serde_json::json!({
        "summary": {
            "total": summary.total(),
            "passed": summary.passed,
            "failed": summary.failed,
            "errored": summary.errored,
        },
        "results": results,
    });
    serde_json::to_string_pretty(&report).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

/// Minimal XML escaping for attribute/text content.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn render_junit(results: &[ScenarioResult]) -> String {
    let summary = Summary::of(results);
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<testsuite name=\"ardur-eval\" tests=\"{}\" failures=\"{}\" errors=\"{}\">\n",
        summary.total(),
        summary.failed,
        summary.errored,
    ));
    for r in results {
        let time = r.duration_ms as f64 / 1000.0;
        out.push_str(&format!(
            "  <testcase name=\"{}\" classname=\"{}\" time=\"{:.3}\">",
            xml_escape(&r.id),
            xml_escape(&r.description),
            time,
        ));
        match &r.outcome {
            Outcome::Pass => {}
            Outcome::Fail { reasons } => {
                out.push('\n');
                out.push_str(&format!(
                    "    <failure message=\"{}\">{}</failure>\n",
                    xml_escape(&reasons.join("; ")),
                    xml_escape(&r.reply),
                ));
                out.push_str("  ");
            }
            Outcome::Error { message } => {
                out.push('\n');
                out.push_str(&format!(
                    "    <error message=\"{}\"/>\n",
                    xml_escape(message),
                ));
                out.push_str("  ");
            }
        }
        out.push_str("</testcase>\n");
    }
    out.push_str("</testsuite>\n");
    out
}

fn render_markdown(results: &[ScenarioResult]) -> String {
    let summary = Summary::of(results);
    let mut out = String::new();
    out.push_str("# Ardur Eval Report\n\n");
    out.push_str(&format!(
        "**{} passed**, **{} failed**, **{} errored** of {} scenarios.\n\n",
        summary.passed,
        summary.failed,
        summary.errored,
        summary.total(),
    ));
    out.push_str("| Scenario | Status | Duration | Detail |\n");
    out.push_str("|---|---|---|---|\n");
    for r in results {
        let (status, detail) = match &r.outcome {
            Outcome::Pass => ("✅ pass".to_string(), String::new()),
            Outcome::Fail { reasons } => ("❌ fail".to_string(), reasons.join("; ")),
            Outcome::Error { message } => ("⚠️ error".to_string(), message.clone()),
        };
        // Keep cell content single-line: escape pipes and collapse newlines.
        let detail = detail.replace('|', "\\|").replace('\n', " ");
        out.push_str(&format!(
            "| {} | {} | {} ms | {} |\n",
            r.id, status, r.duration_ms, detail,
        ));
    }
    out
}
