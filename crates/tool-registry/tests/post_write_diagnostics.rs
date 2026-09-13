//! Post-write diagnostics (gh#414).
//!
//! These tests are written before the implementation and are expected to fail
//! until it lands.
//!
//! The central semantic, pinned here rather than left to the implementation:
//! **diagnostics are advisory**. By the time a checker runs, the bytes are
//! already on disk. Returning `Err` would tell the caller the write failed
//! when it in fact succeeded, and a model that retries on that error would
//! write the same content twice. So a file that fails its check still reports
//! a successful write, with the problems attached.

use ardur_tool_registry::diagnostics::{Diagnostic, DiagnosticSet, Severity, SyntaxCheckers};
use ardur_tool_registry::{
    BuiltinOpts, CapTokenRef, InvocationId, SessionId, Tool, ToolContext, ToolId, ToolRegistry,
    WriteFileTool,
};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;

fn ctx() -> ToolContext {
    ToolContext {
        cap_token: CapTokenRef(String::new()),
        session_id: SessionId::new(),
        invocation_id: InvocationId::new(),
        cwd: PathBuf::from("."),
        env: HashMap::new(),
        cost_budget_cents: u32::MAX,
    }
}

/// Invalid JSON is reported, and the write still succeeds.
#[tokio::test]
async fn a_syntax_error_is_reported_but_the_write_still_succeeds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tool = WriteFileTool::with_root(dir.path().to_path_buf())
        .with_diagnostics(SyntaxCheckers::builtin());

    let out = tool
        .invoke(
            &ctx(),
            json!({ "path": "broken.json", "content": "{\"a\": }" }),
        )
        .await
        .expect("a syntax error must NOT fail the write: the bytes are already on disk");

    // The bytes really are there.
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("broken.json"))
            .await
            .expect("the file was written"),
        "{\"a\": }"
    );

    let diags = &out.content["diagnostics"];
    assert_eq!(
        diags.as_array().map(Vec::len),
        Some(1),
        "expected exactly one diagnostic, got {diags}"
    );
    assert_eq!(diags[0]["severity"], "error");
    assert_eq!(
        diags[0]["source"], "json",
        "the checker that produced it must be named, so a reader can tell a \
         syntax error from a semantic one"
    );
    assert!(
        diags[0]["line"].is_number(),
        "a diagnostic without a location is not actionable: {}",
        diags[0]
    );
}

/// Valid content produces no diagnostics at all.
///
/// The contrast case. Without it, a checker that reported an error on
/// *everything* would pass the test above.
#[tokio::test]
async fn valid_content_reports_no_diagnostics() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tool = WriteFileTool::with_root(dir.path().to_path_buf())
        .with_diagnostics(SyntaxCheckers::builtin());

    let out = tool
        .invoke(
            &ctx(),
            json!({ "path": "fine.json", "content": "{\"a\": 1}" }),
        )
        .await
        .expect("the write succeeds");

    assert!(
        out.content.get("diagnostics").is_none(),
        "a clean file must not carry an empty diagnostics key: {}",
        out.content
    );
}

/// TOML is checked too, and the error names the TOML checker.
#[tokio::test]
async fn toml_is_checked_by_its_own_checker() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tool = WriteFileTool::with_root(dir.path().to_path_buf())
        .with_diagnostics(SyntaxCheckers::builtin());

    let out = tool
        .invoke(
            &ctx(),
            json!({ "path": "bad.toml", "content": "key = = 1" }),
        )
        .await
        .expect("the write succeeds");

    assert_eq!(out.content["diagnostics"][0]["source"], "toml");
    assert_eq!(out.content["diagnostics"][0]["severity"], "error");
}

/// A file type nothing understands is left alone.
///
/// Silence must mean "not checked", never "checked and clean" — the two are
/// different claims and conflating them would let an unchecked file read as
/// validated.
#[tokio::test]
async fn an_unknown_extension_is_not_checked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tool = WriteFileTool::with_root(dir.path().to_path_buf())
        .with_diagnostics(SyntaxCheckers::builtin());

    let out = tool
        .invoke(
            &ctx(),
            json!({ "path": "notes.xyz", "content": "{[(not any language" }),
        )
        .await
        .expect("the write succeeds");

    assert!(out.content.get("diagnostics").is_none());
    assert_eq!(
        out.content["checked"], false,
        "the output must distinguish `not checked` from `checked and clean`"
    );
}

/// Without diagnostics configured, the tool behaves exactly as before.
#[tokio::test]
async fn without_diagnostics_the_output_is_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tool = WriteFileTool::with_root(dir.path().to_path_buf());

    let out = tool
        .invoke(
            &ctx(),
            json!({ "path": "broken.json", "content": "{\"a\": }" }),
        )
        .await
        .expect("the write succeeds");

    assert!(out.content.get("diagnostics").is_none());
    assert!(
        out.content.get("checked").is_none(),
        "an un-opted deployment must see no new keys at all: {}",
        out.content
    );
}

/// The feature is reachable through the real registration path.
///
/// Review caught exactly this omission on gh#413, gh#455 and gh#453: an opt-in
/// builder with no registration path calling it is unreachable in every
/// shipped binary, however the operator configures things.
#[tokio::test]
async fn register_builtins_wires_diagnostics_into_file_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    tokio::fs::create_dir_all(&root).await.expect("root");

    let mut registry = ToolRegistry::new();
    registry
        .register_builtins(BuiltinOpts {
            file_root: Some(root.clone()),
            diagnostics: Some(SyntaxCheckers::builtin()),
            ..Default::default()
        })
        .expect("builtins register");

    let out = registry
        .get(&ToolId::new("file.write"))
        .expect("file.write is registered")
        .invoke(&ctx(), json!({ "path": "bad.json", "content": "{" }))
        .await
        .expect("the write succeeds");

    assert!(
        out.content.get("diagnostics").is_some(),
        "diagnostics configured through BuiltinOpts must actually run: {}",
        out.content
    );
}

/// A checker that panics or hangs must not take the write down with it.
///
/// Checkers are the seam where real language servers will eventually plug in,
/// and those are external processes that crash and hang. The write has already
/// happened, so a broken checker must degrade to "no diagnostics", never to a
/// failed tool call.
#[tokio::test]
async fn a_failing_checker_degrades_to_no_diagnostics() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tool = WriteFileTool::with_root(dir.path().to_path_buf()).with_diagnostics(
        SyntaxCheckers::from_fn("boom", |_path, _content| {
            Err("this checker is broken".to_string())
        }),
    );

    let out = tool
        .invoke(&ctx(), json!({ "path": "any.json", "content": "{}" }))
        .await
        .expect("a broken checker must not fail the write");

    assert!(out.content.get("diagnostics").is_none());
    assert_eq!(
        out.content["checked"], false,
        "a checker that errored did not check anything, so `checked` must be false"
    );
}

/// Diagnostics are truncated, and say so.
///
/// A generated file can produce thousands of errors; pasting all of them into
/// the model's context would crowd out the work. Truncating silently would be
/// worse than truncating loudly, because the reader could not tell a clean
/// tail from a dropped one.
#[tokio::test]
async fn a_flood_of_diagnostics_is_capped_and_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tool = WriteFileTool::with_root(dir.path().to_path_buf()).with_diagnostics(
        SyntaxCheckers::from_fn("flood", |_path, _content| {
            Ok(DiagnosticSet::new(
                (0..500)
                    .map(|i| Diagnostic {
                        severity: Severity::Error,
                        line: Some(i + 1),
                        column: None,
                        message: format!("problem {i}"),
                    })
                    .collect(),
            ))
        }),
    );

    let out = tool
        .invoke(&ctx(), json!({ "path": "any.json", "content": "{}" }))
        .await
        .expect("the write succeeds");

    let diags = out.content["diagnostics"].as_array().expect("an array");
    assert!(
        diags.len() <= 50,
        "expected the list to be capped, got {}",
        diags.len()
    );
    assert_eq!(
        out.content["diagnostics_truncated"],
        json!(500 - diags.len())
    );
}

/// An append is checked against the whole file, not the added fragment.
///
/// Found in self-review: the first implementation checked `args.content`,
/// which in append mode is only the chunk being added. A JSON fragment almost
/// never parses on its own, so every append to a perfectly valid file reported
/// a syntax error — and breakage the append genuinely caused went unseen,
/// because the fragment alone looked fine.
#[tokio::test]
async fn an_append_is_checked_against_the_resulting_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    tokio::fs::write(dir.path().join("data.json"), "{\"a\": 1")
        .await
        .expect("seed a file that is not yet valid");

    let tool = WriteFileTool::with_root(dir.path().to_path_buf())
        .with_diagnostics(SyntaxCheckers::builtin());

    // The fragment "}" is not valid JSON by itself, but completes the file.
    let out = tool
        .invoke(
            &ctx(),
            json!({ "path": "data.json", "content": "}", "mode": "append" }),
        )
        .await
        .expect("the write succeeds");

    assert_eq!(out.content["checked"], true);
    assert!(
        out.content.get("diagnostics").is_none(),
        "the completed file is valid JSON; checking the fragment alone would \
         have reported a spurious error: {}",
        out.content
    );
}

/// An append that breaks the file is reported.
///
/// The contrast case: without it, a checker that ignored appends entirely
/// would pass the test above.
#[tokio::test]
async fn an_append_that_breaks_the_file_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    tokio::fs::write(dir.path().join("data.json"), "{\"a\": 1}")
        .await
        .expect("seed a valid file");

    let tool = WriteFileTool::with_root(dir.path().to_path_buf())
        .with_diagnostics(SyntaxCheckers::builtin());

    let out = tool
        .invoke(
            &ctx(),
            json!({ "path": "data.json", "content": "garbage", "mode": "append" }),
        )
        .await
        .expect("the write succeeds");

    assert_eq!(out.content["diagnostics"][0]["source"], "json");
    assert_eq!(out.content["diagnostics"][0]["severity"], "error");
}

/// A checker that PANICS must not fail the write.
///
/// Review P1: the earlier panic test only exercised an ordinary `Err`, so an
/// unwinding checker was uncovered. Checkers are the seam where external
/// language servers plug in, and those crash. In append mode a failed
/// invocation is worse than cosmetic — a caller retrying would append the
/// bytes a second time.
#[tokio::test]
async fn a_panicking_checker_does_not_fail_the_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tool = WriteFileTool::with_root(dir.path().to_path_buf()).with_diagnostics(
        SyntaxCheckers::from_fn("panicker", |_path, _content| {
            panic!("this checker exploded");
        }),
    );

    let out = tool
        .invoke(&ctx(), json!({ "path": "a.json", "content": "{}" }))
        .await
        .expect("a panicking checker must not fail the write");

    assert_eq!(
        out.content["checked"], false,
        "a checker that panicked did not check anything"
    );
    assert_eq!(
        tokio::fs::read_to_string(dir.path().join("a.json"))
            .await
            .expect("the file exists"),
        "{}"
    );
}

/// Diagnostics reach receipt_data, not just the model-visible content.
///
/// Review P2: `output()` mirrors content into `receipt_data`, and inserting
/// into only one left the receipt auditing a different result than the model
/// saw — which defeats the purpose of the receipt.
#[tokio::test]
async fn diagnostics_are_mirrored_into_receipt_data() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tool = WriteFileTool::with_root(dir.path().to_path_buf())
        .with_diagnostics(SyntaxCheckers::builtin());

    let out = tool
        .invoke(&ctx(), json!({ "path": "bad.json", "content": "{" }))
        .await
        .expect("the write succeeds");

    assert_eq!(
        out.receipt_data["diagnostics"], out.content["diagnostics"],
        "the receipt must record what the model was told"
    );
    assert_eq!(out.receipt_data["checked"], out.content["checked"]);
    // And the clean case still mirrors `checked`.
    let clean = tool
        .invoke(&ctx(), json!({ "path": "ok.json", "content": "{}" }))
        .await
        .expect("the write succeeds");
    assert_eq!(clean.receipt_data["checked"], json!(true));
}

/// An external checker can be registered and composed with the built-ins.
///
/// Review P2: `Checker` was public and documented as the extension point, but
/// nothing outside the crate could construct a set containing one — which made
/// "extension point" a claim the API did not support.
#[tokio::test]
async fn an_external_checker_composes_with_the_builtins() {
    #[derive(Debug)]
    struct PyChecker;

    impl ardur_tool_registry::diagnostics::Checker for PyChecker {
        fn name(&self) -> &str {
            "py"
        }
        fn handles(&self, path: &std::path::Path) -> bool {
            path.extension().and_then(|e| e.to_str()) == Some("py")
        }
        fn check(&self, _path: &std::path::Path, content: &str) -> Result<DiagnosticSet, String> {
            if content.contains("import os") {
                Ok(DiagnosticSet::new(vec![Diagnostic {
                    severity: Severity::Warning,
                    line: Some(1),
                    column: None,
                    message: "os is discouraged here".to_string(),
                }]))
            } else {
                Ok(DiagnosticSet::clean())
            }
        }
        fn boxed_clone(&self) -> Box<dyn ardur_tool_registry::diagnostics::Checker> {
            Box::new(PyChecker)
        }
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let checkers = SyntaxCheckers::builtin().with_checker(Box::new(PyChecker));
    let tool = WriteFileTool::with_root(dir.path().to_path_buf()).with_diagnostics(checkers);

    // The external checker fires.
    let out = tool
        .invoke(&ctx(), json!({ "path": "s.py", "content": "import os\n" }))
        .await
        .expect("the write succeeds");
    assert_eq!(out.content["diagnostics"][0]["source"], "py");

    // And the built-ins still work alongside it.
    let json_out = tool
        .invoke(&ctx(), json!({ "path": "b.json", "content": "{" }))
        .await
        .expect("the write succeeds");
    assert_eq!(json_out.content["diagnostics"][0]["source"], "json");
}
