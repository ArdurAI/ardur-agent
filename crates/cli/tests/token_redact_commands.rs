//! Integration tests for `ardur token` and `ardur redact`.

use assert_cmd::Command;
use std::fs;

#[test]
fn token_create_lists_and_revokes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join(".ardur");

    // Create should print a value and an ID, and write a record.
    let create = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["token", "create", "test-label"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let create_stdout = String::from_utf8(create).expect("stdout utf8");
    assert!(
        create_stdout.contains("created token"),
        "create should confirm: {create_stdout}"
    );
    assert!(
        create_stdout.contains("value:") && create_stdout.contains("warning:"),
        "create should show value once: {create_stdout}"
    );

    // Extract token id from stdout line like "created token <uuid>".
    let id = create_stdout
        .lines()
        .find(|l| l.starts_with("created token"))
        .and_then(|l| l.split_whitespace().nth(2))
        .expect("token id in stdout")
        .to_string();

    // List should succeed and redact hash field.
    let list = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["token", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_stdout = String::from_utf8(list).expect("stdout utf8");
    assert!(list_stdout.contains("test-label"), "{list_stdout}");
    assert!(list_stdout.contains("<redacted>"), "{list_stdout}");

    // Revoke should mark the record revoked.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["token", "revoke", &id])
        .assert()
        .success();

    let record_path = state.join("tokens").join(format!("{id}.json"));
    let record = fs::read_to_string(record_path).expect("record file");
    let json: serde_json::Value = serde_json::from_str(&record).expect("record json");
    assert_eq!(json["revoked"], true);
    assert!(json["revoked_at"].is_number());
}

#[test]
fn redact_plain_text_masks_default_patterns() {
    let anthropic = format!("sk-ant-api03-{}", "a".repeat(32));
    let openrouter = format!("sk-or-v1-{}", "b".repeat(32));
    let input = format!("My keys are {anthropic} and {openrouter}; pass is secret123");
    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .args(["redact"])
        .write_stdin(input)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout utf8");
    assert!(
        stdout.contains("<REDACTED>"),
        "should redact secrets: {stdout}"
    );
    assert!(
        !stdout.contains(&anthropic),
        "Anthropic key leaked: {stdout}"
    );
    assert!(
        !stdout.contains(&openrouter),
        "OpenRouter key leaked: {stdout}"
    );
    assert!(!stdout.contains("secret123"), "password leaked: {stdout}");
}

#[test]
fn redact_json_recursively_masks_string_values() {
    let input = r#"{"api_key":"sk-abcdefghijklmnopqrstuvwxyz1234567890abc","nested":{"password":"secret123"}}"#;
    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .args(["redact", "--json"])
        .write_stdin(input)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout utf8");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("json output");
    assert_eq!(json["api_key"], "<REDACTED>");
    assert_eq!(json["nested"]["password"], "<REDACTED>");
}

#[test]
fn redact_custom_pattern_can_be_added() {
    let input = "My internal code is XYZZY-9999-0000";
    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .args(["redact", "--pattern", r"XYZZY-\d{4}-\d{4}"])
        .write_stdin(input)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout utf8");
    assert!(stdout.contains("<REDACTED>"), "{stdout}");
    assert!(
        !stdout.contains("XYZZY-9999-0000"),
        "custom secret leaked: {stdout}"
    );
}

#[test]
fn redact_input_output_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input_path = dir.path().join("input.txt");
    let output_path = dir.path().join("output.txt");
    fs::write(
        &input_path,
        "token=ghp_000000000000000000000000000000000000",
    )
    .expect("write input");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .args([
            "redact",
            "-i",
            input_path.to_str().unwrap(),
            "-o",
            output_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    let result = fs::read_to_string(&output_path).expect("read output");
    assert!(result.contains("<REDACTED>"), "{result}");
    assert!(
        !result.contains("ghp_000000000000000000000000000000000000"),
        "{result}"
    );
}
