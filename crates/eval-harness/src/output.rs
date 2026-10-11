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
    /// Number of scenarios lacking required evidence.
    pub unavailable: usize,
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
                Outcome::Unavailable { .. } => s.unavailable += 1,
            }
        }
        s
    }

    /// Total scenarios tallied.
    pub fn total(&self) -> usize {
        self.passed + self.failed + self.errored + self.unavailable
    }

    /// True only when every scenario passed with the required evidence.
    pub fn is_green(&self) -> bool {
        self.failed == 0 && self.errored == 0 && self.unavailable == 0
    }
}

/// Render `results` in the requested `format`.
pub fn render(results: &[ScenarioResult], format: Format) -> String {
    let safe: Vec<_> = results.iter().cloned().map(sanitize).collect();
    let results = safe.as_slice();
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
            "unavailable": summary.unavailable,
        },
        "results": results,
    });
    serde_json::to_string_pretty(&report).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

/// Minimal XML escaping for attribute/text content.
fn xml_escape(s: &str) -> String {
    // XML 1.0 excludes two noncharacters that are valid Rust scalar values.
    s.chars()
        .filter(|c| !matches!(c, '\u{fffe}' | '\u{ffff}'))
        .collect::<String>()
        .replace('&', "&amp;")
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
        "<testsuite name=\"ardur-eval\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\">\n",
        summary.total(),
        summary.failed,
        summary.errored,
        summary.unavailable,
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
            Outcome::Unavailable { reasons } => {
                out.push_str(&format!(
                    "<skipped message=\"{}\"/>",
                    xml_escape(&reasons.join("; "))
                ));
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
        "**{} passed**, **{} failed**, **{} errored**, **{} unavailable** of {} scenarios.\n\n",
        summary.passed,
        summary.failed,
        summary.errored,
        summary.unavailable,
        summary.total(),
    ));
    out.push_str("| Scenario | Status | Duration | Detail |\n");
    out.push_str("|---|---|---|---|\n");
    for r in results {
        let (status, detail) = match &r.outcome {
            Outcome::Pass => ("✅ pass".to_string(), String::new()),
            Outcome::Fail { reasons } => ("❌ fail".to_string(), reasons.join("; ")),
            Outcome::Error { message } => ("⚠️ error".to_string(), message.clone()),
            Outcome::Unavailable { reasons } => ("unavailable".to_string(), reasons.join("; ")),
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

// Sanitize fields before escaping XML or JSON; serialized escapes can hide secrets.
fn sanitize(mut result: ScenarioResult) -> ScenarioResult {
    use home_client::safe_output;
    result.id = safe_output(&result.id);
    result.description = safe_output(&result.description);
    result.reply = safe_output(&result.reply);
    match &mut result.outcome {
        Outcome::Fail { reasons } | Outcome::Unavailable { reasons } => {
            for reason in reasons {
                *reason = safe_output(reason);
            }
        }
        Outcome::Error { message } => *message = safe_output(message),
        Outcome::Pass => {}
    }
    result
}
