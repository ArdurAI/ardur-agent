//! Sentinel scan / secret redaction for rendered text (§9.4, ADR-Phase3-280).
//!
//! Every text field that reaches the terminal — cron name, prompt, delivery
//! config, run output/error — passes through [`Redactor::scan`] first. Hits
//! collapse to a `<redacted>` placeholder so a credential planted in a cron
//! arg never displays verbatim.
//!
//! The scan is rule-based (no LLM in the render path). Patterns cover the
//! common credential shapes: provider API keys (`sk-...`, `AKIA...`), bearer
//! tokens, JWT-shaped triples, and long hex/base64 secret blobs.

use std::borrow::Cow;

use regex::Regex;

/// The placeholder that replaces a matched secret.
pub const REDACTED: &str = "<redacted>";

/// A reusable, rule-based secret scanner.
#[derive(Debug, Clone)]
pub struct Redactor {
    patterns: Vec<Regex>,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

impl Redactor {
    /// Build a redactor with the default credential pattern set.
    pub fn new() -> Self {
        // Each pattern targets a distinct credential shape. Order does not
        // matter — every pattern is applied in turn.
        let raw = [
            // OpenAI-style keys: sk-, sk-proj-, rk-, plus a long body.
            r"(?i)\b(?:sk|rk)-[A-Za-z0-9_-]{16,}\b",
            // AWS access key id.
            r"\bAKIA[0-9A-Z]{16}\b",
            // GitHub tokens.
            r"\bgh[posru]_[A-Za-z0-9]{20,}\b",
            // Slack tokens.
            r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b",
            // Bearer tokens in a header-ish context.
            r"(?i)\bbearer\s+[A-Za-z0-9._-]{16,}\b",
            // JWT-shaped triples (three base64url segments).
            r"\beyJ[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}\b",
            // Long hex blobs (>= 32 hex chars) — API secrets / signing keys.
            r"\b[0-9a-fA-F]{32,}\b",
            // `key = value` / `token: value` assignments with a secret-ish value.
            r"(?i)\b(?:api[_-]?key|secret|token|password|passwd)\b\s*[:=]\s*\S{8,}",
        ];
        let patterns = raw
            .into_iter()
            .map(|p| Regex::new(p).expect("valid redaction regex"))
            .collect();
        Self { patterns }
    }

    /// Scan `text`, collapsing every matched secret to [`REDACTED`]. Returns a
    /// borrowed `Cow` when nothing matched (the common case).
    pub fn scan<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out: Cow<'a, str> = Cow::Borrowed(text);
        for re in &self.patterns {
            if re.is_match(&out) {
                let replaced = re.replace_all(&out, REDACTED).into_owned();
                out = Cow::Owned(replaced);
            }
        }
        out
    }

    /// Convenience: scan an optional field.
    pub fn scan_opt(&self, text: &Option<String>) -> Option<String> {
        text.as_ref().map(|t| self.scan(t).into_owned())
    }
}
