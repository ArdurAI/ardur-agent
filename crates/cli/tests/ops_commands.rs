//! CLI expansion smoke tests for `ardur config`, `ardur logs`, `ardur debug`,
//! and `ardur doctor`.

use assert_cmd::Command;
use std::fs;

#[test]
fn config_view_redacts_secret_and_config_set_edits_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    fs::write(
        &config_path,
        "api_key = \"sk-live-secret\"\nmodel = \"claude-old\"\nbudget_cents = 123\n",
    )
    .expect("write config");

    let view = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .args(["config", "--config"])
        .arg(&config_path)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(view).expect("stdout utf8");
    assert!(stdout.contains("\"api_key_present\": true"), "{stdout}");
    assert!(stdout.contains("\"model\": \"claude-old\""), "{stdout}");
    assert!(
        !stdout.contains("sk-live-secret"),
        "config view leaked API key: {stdout}"
    );

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .args(["config", "--config"])
        .arg(&config_path)
        .args(["set", "model", "claude-new"])
        .assert()
        .success();

    let edited = fs::read_to_string(&config_path).expect("read edited config");
    assert!(edited.contains("model = \"claude-new\""), "{edited}");
    assert!(edited.contains("budget_cents = 123"), "{edited}");
    assert!(edited.contains("api_key = \"sk-live-secret\""), "{edited}");
}

#[test]
fn logs_tails_structured_logs_and_redacts_secret_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let logs_dir = dir.path().join("logs");
    fs::create_dir_all(&logs_dir).expect("create logs dir");
    fs::write(
        logs_dir.join("ardur.log"),
        concat!(
            "{\"level\":\"INFO\",\"message\":\"first\"}\n",
            "{\"level\":\"WARN\",\"api_key\":\"sk-log-secret\",\"message\":\"second\"}\n",
            "{\"level\":\"INFO\",\"token\":\"xoxb-log-secret\",\"message\":\"third\"}\n",
        ),
    )
    .expect("write log");

    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .args(["logs", "--dir"])
        .arg(dir.path())
        .args(["--lines", "2"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout utf8");
    assert!(
        !stdout.contains("first"),
        "tail should only include last two lines: {stdout}"
    );
    assert!(
        stdout.contains("second") && stdout.contains("third"),
        "{stdout}"
    );
    assert!(stdout.contains("<redacted>"), "{stdout}");
    assert!(
        !stdout.contains("sk-log-secret") && !stdout.contains("xoxb-log-secret"),
        "{stdout}"
    );
}

#[test]
fn debug_dumps_runtime_state_without_key_material() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("keys")).expect("keys dir");
    fs::create_dir_all(dir.path().join("receipts")).expect("receipts dir");
    fs::write(dir.path().join("keys/issuer.key"), "issuer-private-secret").expect("issuer key");
    fs::write(dir.path().join("receipts/chain.jsonl"), "{}\n{}\n").expect("receipts");

    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .args(["debug", "--state-dir"])
        .arg(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout utf8");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("debug json");
    assert_eq!(json["receipts"]["count"], 2);
    assert_eq!(json["keys"]["issuer_key_present"], true);
    assert!(
        !stdout.contains("issuer-private-secret"),
        "debug leaked key material: {stdout}"
    );
}

#[test]
fn doctor_reports_ok_and_detects_missing_required_api_key() {
    let dir = tempfile::tempdir().expect("tempdir");

    let ok = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .args(["doctor", "--state-dir"])
        .arg(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let ok_stdout = String::from_utf8(ok).expect("stdout utf8");
    assert!(ok_stdout.contains("\"status\": \"ok\""), "{ok_stdout}");

    let failed = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env_remove("ANTHROPIC_API_KEY")
        .args(["doctor", "--state-dir"])
        .arg(dir.path())
        .arg("--require-api-key")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let failed_stderr = String::from_utf8(failed).expect("stderr utf8");
    assert!(
        failed_stderr.contains("doctor found issues"),
        "{failed_stderr}"
    );
}
