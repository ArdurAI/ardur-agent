//! Black-box CLI tests: drive the built `ardur-eval` binary end-to-end.
//!
//! `cli_run_outputs_junit` stands up a `wiremock` `/chat` endpoint and runs the
//! `run` subcommand against it; `cli_list_shows_scenarios` and `cli_new_*`
//! exercise the offline subcommands over a tempdir.

use std::fs;

use assert_cmd::Command;
use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn write_scenario(dir: &std::path::Path, name: &str, body: &str) {
    fs::write(dir.join(name), body).expect("write scenario");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_run_outputs_junit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "reply": "The capital is Paris.",
        })))
        .mount(&server)
        .await;
    let uri = server.uri();

    let dir = TempDir::new().expect("tempdir");
    write_scenario(
        dir.path(),
        "cap.yaml",
        "id: cap\ndescription: capital\nprompt: \"capital of France?\"\nexpected:\n  contains: [\"Paris\"]\n",
    );
    let scenarios = dir.path().to_path_buf();

    let output = tokio::task::spawn_blocking(move || {
        Command::cargo_bin("ardur-eval")
            .expect("binary")
            .args([
                "run",
                "--scenarios",
                scenarios.to_str().unwrap(),
                "--server-url",
                &uri,
                "--output",
                "junit",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    })
    .await
    .expect("join");

    let stdout = String::from_utf8(output).expect("utf8");
    assert!(stdout.contains("<testsuite"), "stdout: {stdout}");
    assert!(stdout.contains("name=\"cap\""), "stdout: {stdout}");
    assert!(stdout.contains("tests=\"1\""), "stdout: {stdout}");
}

#[test]
fn cli_list_shows_scenarios() {
    let dir = TempDir::new().expect("tempdir");
    write_scenario(
        dir.path(),
        "alpha.yaml",
        "id: alpha\ndescription: first\nprompt: \"hi\"\n",
    );
    write_scenario(
        dir.path(),
        "beta.yaml",
        "id: beta\ndescription: second\nprompt: \"yo\"\n",
    );

    let output = Command::cargo_bin("ardur-eval")
        .expect("binary")
        .args(["list", "--scenarios", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf8");
    assert!(stdout.contains("alpha"), "stdout: {stdout}");
    assert!(stdout.contains("beta"), "stdout: {stdout}");
    assert!(stdout.contains("2 scenario(s)"), "stdout: {stdout}");
}

#[test]
fn cli_new_scaffolds_file() {
    let dir = TempDir::new().expect("tempdir");
    Command::cargo_bin("ardur-eval")
        .expect("binary")
        .args([
            "new",
            "--id",
            "smoke",
            "--scenarios",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let written = fs::read_to_string(dir.path().join("smoke.yaml")).expect("scaffolded file");
    assert!(written.contains("id: smoke"), "yaml: {written}");
}
