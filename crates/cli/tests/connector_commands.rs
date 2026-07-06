//! Integration tests for `ardur fetch` and `ardur search`.

use assert_cmd::Command;

#[test]
fn fetch_rejects_non_allowed_host() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Without allowlist, httpbin.org should be rejected because the default
    // allowlist file is empty.
    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args([
            "fetch",
            "https://example.com/index.html",
            "--max-bytes",
            "1024",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8_lossy(&output);
    assert!(stderr.contains("allowlist"), "stderr: {stderr}");
}

#[test]
fn fetch_allows_explicit_host() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Even with --allow-host, the URL scheme still matters; this test just
    // checks that the allowlist gate passes and the network request is
    // attempted (it may fail without connectivity, but not on allowlist).
    let assert = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args([
            "fetch",
            "https://example.com/index.html",
            "--allow-host",
            "example.com",
            "--max-bytes",
            "1024",
        ])
        .assert();

    // Either success or a network error is acceptable; the important thing is
    // we did not get the allowlist error.
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        !stderr.contains("allowlist"),
        "should pass allowlist gate: {stderr}"
    );
}

#[test]
fn fetch_rejects_bad_scheme() {
    let dir = tempfile::tempdir().expect("tempdir");

    let assert = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["fetch", "ftp://example.com/file"])
        .assert()
        .failure();
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("http://"), "stderr: {stderr}");
}

#[test]
fn search_stub_succeeds() {
    let assert = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .args([
            "search",
            "rust programming",
            "--provider",
            "web",
            "--limit",
            "5",
        ])
        .assert()
        .success();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("search:"), "stdout: {stdout}");
}
