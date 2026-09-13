//! Post-write diagnostics for file-writing tools (gh#414).
//!
//! # Why these are advisory
//!
//! A checker runs *after* the bytes are on disk. Returning an error from the
//! tool would report a failed write that actually succeeded, and a model that
//! retries on failure would then write the same content a second time. So a
//! file that fails its check still reports success, with the problems attached
//! to the output for the model to read and act on.
//!
//! # What this is not
//!
//! gh#414 asks for real language servers — pyright, gopls, rust-analyzer.
//! This module is the *extension point* they will plug into, plus two
//! dependency-free syntax checkers as its first implementations. Running real
//! language servers additionally requires an LSP/JSON-RPC client (no such
//! crate is in this workspace), host language-server binaries, and process
//! spawning that must go through the gh#420 argv-exec confinement. None of
//! that is built here, and the docs say so rather than implying coverage this
//! does not have.

use std::sync::Arc;

/// How serious a diagnostic is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The file is wrong — it will not parse, compile, or load.
    Error,
    /// The file is valid but questionable.
    Warning,
}

impl Severity {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// One problem found in a written file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// How serious it is.
    pub severity: Severity,
    /// 1-based line, when the checker reports one.
    pub line: Option<usize>,
    /// 1-based column, when the checker reports one.
    pub column: Option<usize>,
    /// What is wrong.
    pub message: String,
}

/// The diagnostics one checker produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagnosticSet {
    items: Vec<Diagnostic>,
}

impl DiagnosticSet {
    /// A set holding `items`.
    #[must_use]
    pub fn new(items: Vec<Diagnostic>) -> Self {
        Self { items }
    }

    /// No problems found.
    #[must_use]
    pub fn clean() -> Self {
        Self { items: Vec::new() }
    }

    /// The diagnostics.
    #[must_use]
    pub fn items(&self) -> &[Diagnostic] {
        &self.items
    }

    /// Whether anything was found.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// The most diagnostics that will be attached to a single write.
///
/// A generated or truncated file can produce thousands. Pasting all of them
/// into the model's context would crowd out the actual work, so the list is
/// capped — and the output reports how many were dropped, because a silently
/// truncated list reads exactly like a clean tail.
pub const MAX_DIAGNOSTICS: usize = 50;

/// The largest file this will read back to check.
///
/// Only relevant for appends, where the content to check is the file on disk
/// rather than the argument already in memory. Without a ceiling, appending a
/// line to a multi-gigabyte log would pull the whole thing into memory to run
/// a syntax check on it.
pub const MAX_CHECK_BYTES: u64 = 8 * 1024 * 1024;

/// Checks the content a tool just wrote.
///
/// `check` returns `Err` when the checker itself failed — it could not run, or
/// crashed. That is reported as "not checked", never as a clean file: silence
/// must not be mistakable for validation.
pub trait Checker: Send + Sync {
    /// A stable name, surfaced so a reader can tell which checker spoke.
    fn name(&self) -> &str;

    /// Whether this checker handles `path`.
    fn handles(&self, path: &std::path::Path) -> bool;

    /// Check `content`.
    ///
    /// # Errors
    ///
    /// Returns a message describing why the check could not run.
    fn check(&self, path: &std::path::Path, content: &str) -> Result<DiagnosticSet, String>;
}

/// A set of checkers, applied to each write.
#[derive(Clone)]
pub struct SyntaxCheckers {
    checkers: Arc<Vec<Box<dyn Checker>>>,
}

impl std::fmt::Debug for SyntaxCheckers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn Checker` has no Debug bound, so list the names instead.
        f.debug_struct("SyntaxCheckers")
            .field(
                "checkers",
                &self.checkers.iter().map(|c| c.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl SyntaxCheckers {
    /// The dependency-free checkers: JSON and TOML.
    ///
    /// Both parsers are already vendored for other purposes, so this adds no
    /// dependency.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            checkers: Arc::new(vec![Box::new(JsonChecker), Box::new(TomlChecker)]),
        }
    }

    /// A single checker built from a closure. Intended for tests and for
    /// embedding a bespoke check without defining a type.
    pub fn from_fn<F>(name: &str, f: F) -> Self
    where
        F: Fn(&std::path::Path, &str) -> Result<DiagnosticSet, String> + Send + Sync + 'static,
    {
        Self {
            checkers: Arc::new(vec![Box::new(FnChecker {
                name: name.to_string(),
                f: Box::new(f),
            })]),
        }
    }

    /// Run every checker that handles `path`.
    ///
    /// Returns the diagnostics found, how many were dropped by the cap, and
    /// whether any checker actually ran. A checker that returns `Err` is
    /// skipped: it did not check the file, so it contributes nothing and must
    /// not make the file look validated.
    #[must_use]
    pub fn run(&self, path: &std::path::Path, content: &str) -> DiagnosticReport {
        let mut found = Vec::new();
        let mut checked = false;

        for checker in self.checkers.iter() {
            if !checker.handles(path) {
                continue;
            }
            match checker.check(path, content) {
                Ok(set) => {
                    checked = true;
                    for d in set.items() {
                        found.push((checker.name().to_string(), d.clone()));
                    }
                }
                // The checker is broken or unavailable. The write already
                // happened, so degrade to "not checked" rather than failing.
                Err(_) => continue,
            }
        }

        let total = found.len();
        found.truncate(MAX_DIAGNOSTICS);
        DiagnosticReport {
            truncated: total - found.len(),
            items: found,
            checked,
        }
    }
}

/// What the checkers had to say about one write.
#[derive(Debug, Clone)]
pub struct DiagnosticReport {
    items: Vec<(String, Diagnostic)>,
    truncated: usize,
    checked: bool,
}

impl DiagnosticReport {
    /// A report meaning "nothing was checked" — the file could not be read
    /// back, or was too large to check. Distinct from a clean check.
    #[must_use]
    pub fn not_checked() -> Self {
        Self {
            items: Vec::new(),
            truncated: 0,
            checked: false,
        }
    }

    /// Whether any checker ran. `false` means the file type is unrecognised or
    /// every applicable checker failed — **not** that the file is clean.
    #[must_use]
    pub fn was_checked(&self) -> bool {
        self.checked
    }

    /// Whether any problems were found.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// How many diagnostics the cap dropped.
    #[must_use]
    pub fn truncated(&self) -> usize {
        self.truncated
    }

    /// The diagnostics as JSON, for attaching to a tool's output.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.items
                .iter()
                .map(|(source, d)| {
                    serde_json::json!({
                        "source": source,
                        "severity": d.severity.as_str(),
                        "line": d.line,
                        "column": d.column,
                        "message": d.message,
                    })
                })
                .collect(),
        )
    }
}

struct FnChecker {
    name: String,
    #[allow(clippy::type_complexity)]
    f: Box<dyn Fn(&std::path::Path, &str) -> Result<DiagnosticSet, String> + Send + Sync>,
}

impl Checker for FnChecker {
    fn name(&self) -> &str {
        &self.name
    }

    fn handles(&self, _path: &std::path::Path) -> bool {
        true
    }

    fn check(&self, path: &std::path::Path, content: &str) -> Result<DiagnosticSet, String> {
        (self.f)(path, content)
    }
}

fn has_extension(path: &std::path::Path, want: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(want))
}

/// Validates `.json` with the vendored `serde_json`.
#[derive(Debug)]
struct JsonChecker;

impl Checker for JsonChecker {
    fn name(&self) -> &str {
        "json"
    }

    fn handles(&self, path: &std::path::Path) -> bool {
        has_extension(path, "json")
    }

    fn check(&self, _path: &std::path::Path, content: &str) -> Result<DiagnosticSet, String> {
        match serde_json::from_str::<serde_json::Value>(content) {
            Ok(_) => Ok(DiagnosticSet::clean()),
            Err(e) => Ok(DiagnosticSet::new(vec![Diagnostic {
                severity: Severity::Error,
                line: Some(e.line()),
                column: Some(e.column()),
                message: e.to_string(),
            }])),
        }
    }
}

/// Validates `.toml` with the vendored `toml` parser.
#[derive(Debug)]
struct TomlChecker;

impl Checker for TomlChecker {
    fn name(&self) -> &str {
        "toml"
    }

    fn handles(&self, path: &std::path::Path) -> bool {
        has_extension(path, "toml")
    }

    fn check(&self, _path: &std::path::Path, content: &str) -> Result<DiagnosticSet, String> {
        match toml::from_str::<toml::Value>(content) {
            Ok(_) => Ok(DiagnosticSet::clean()),
            Err(e) => {
                // `toml` reports a byte span; convert its start to a 1-based
                // line/column so the diagnostic is actionable.
                let (line, column) = e
                    .span()
                    .map(|s| offset_to_line_col(content, s.start))
                    .map_or((None, None), |(l, c)| (Some(l), Some(c)));
                // `e.message()` ONLY — never `e.to_string()`. The `Display`
                // impl renders a source excerpt with the offending line, so a
                // syntax error on a line holding a credential would copy that
                // credential into the model's context and the receipt. Proven
                // against the real parser; pinned by
                // `a_toml_error_never_echoes_the_offending_line`.
                Ok(DiagnosticSet::new(vec![Diagnostic {
                    severity: Severity::Error,
                    line,
                    column,
                    message: e.message().to_string(),
                }]))
            }
        }
    }
}

/// Convert a byte offset into 1-based line and column.
fn offset_to_line_col(content: &str, offset: usize) -> (usize, usize) {
    let upto = &content[..offset.min(content.len())];
    let line = upto.matches('\n').count() + 1;
    let column = upto
        .rsplit_once('\n')
        .map_or(upto.len(), |(_, tail)| tail.len())
        + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn json_errors_carry_a_location() {
        let report = SyntaxCheckers::builtin().run(Path::new("a.json"), "{\n  \"a\": }\n");
        assert!(report.was_checked());
        assert!(!report.is_empty());
        let json = report.to_json();
        assert_eq!(json[0]["source"], "json");
        assert_eq!(json[0]["line"], 2);
    }

    #[test]
    fn valid_json_is_clean_and_checked() {
        let report = SyntaxCheckers::builtin().run(Path::new("a.json"), "{\"a\": 1}");
        assert!(report.was_checked(), "a .json file IS handled");
        assert!(report.is_empty());
    }

    #[test]
    fn an_unhandled_extension_reports_not_checked() {
        let report = SyntaxCheckers::builtin().run(Path::new("a.xyz"), "not json at all {[(");
        assert!(
            !report.was_checked(),
            "nothing handled this file, so it must not read as validated"
        );
        assert!(report.is_empty());
    }

    #[test]
    fn a_broken_checker_reports_not_checked() {
        let checkers = SyntaxCheckers::from_fn("boom", |_, _| Err("unavailable".to_string()));
        let report = checkers.run(Path::new("a.json"), "{}");
        assert!(
            !report.was_checked(),
            "a checker that could not run has not validated anything"
        );
        assert!(report.is_empty());
    }

    #[test]
    fn the_list_is_capped_and_reports_the_drop() {
        let checkers = SyntaxCheckers::from_fn("flood", |_, _| {
            Ok(DiagnosticSet::new(
                (0..120)
                    .map(|i| Diagnostic {
                        severity: Severity::Warning,
                        line: Some(i + 1),
                        column: None,
                        message: "noise".to_string(),
                    })
                    .collect(),
            ))
        });
        let report = checkers.run(Path::new("a.json"), "{}");
        assert_eq!(report.to_json().as_array().map(Vec::len), Some(50));
        assert_eq!(report.truncated(), 70);
    }

    #[test]
    fn toml_errors_carry_a_location() {
        let report = SyntaxCheckers::builtin().run(Path::new("a.toml"), "ok = 1\nbad = = 2\n");
        let json = report.to_json();
        assert_eq!(json[0]["source"], "toml");
        assert_eq!(json[0]["line"], 2);
    }

    /// A TOML error must not echo the line it failed on.
    ///
    /// `toml::de::Error`'s `Display` renders a source excerpt including that
    /// line. Config files are exactly where credentials live, so a syntax
    /// error on a secret-bearing line would copy the secret into the model's
    /// context. Verified against the real parser: `Display` contains the
    /// secret, `message()` does not.
    #[test]
    fn a_toml_error_never_echoes_the_offending_line() {
        let secret = "sk-SUPERSECRET-1234";
        let content = format!("api_key = \"{secret}\" = oops\n");
        let report = SyntaxCheckers::builtin().run(Path::new("c.toml"), &content);

        assert!(!report.is_empty(), "this really is a syntax error");
        let rendered = report.to_json().to_string();
        assert!(
            !rendered.contains(secret),
            "the diagnostic leaked the secret on the offending line: {rendered}"
        );
    }

    /// The same guarantee for JSON.
    #[test]
    fn a_json_error_never_echoes_the_offending_content() {
        let secret = "sk-SUPERSECRET-1234";
        let content = format!("{{\"api_key\": \"{secret}\" \"next\": 1}}");
        let report = SyntaxCheckers::builtin().run(Path::new("c.json"), &content);

        assert!(!report.is_empty(), "this really is a syntax error");
        assert!(!report.to_json().to_string().contains(secret));
    }

    #[test]
    fn offsets_convert_to_one_based_positions() {
        assert_eq!(offset_to_line_col("abc", 0), (1, 1));
        assert_eq!(offset_to_line_col("abc\ndef", 4), (2, 1));
        assert_eq!(offset_to_line_col("abc\ndef", 6), (2, 3));
    }
}
