//! Binary-level provider selection error handling for `ardur-server`.
//!
//! The config layer deliberately does not reject unknown `ARDUR_PROVIDER` values
//! because it only decides whether an Anthropic key is required. The binary must
//! then reject the bad selector cleanly before binding a socket.

use assert_cmd::Command;

#[test]
fn server_invalid_provider_exits_with_clean_error() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let output = Command::cargo_bin("ardur-server")
        .expect("the `ardur-server` binary builds")
        .env("ARDUR_PROVIDER", "mistral")
        .env_remove("ANTHROPIC_API_KEY")
        .env("SLACK_BOT_TOKEN", "xoxb-server-test-token")
        .env("SLACK_SIGNING_SECRET", "server-signing-secret-000000000000")
        .env("SLACK_APP_ID", "A0SERVERTEST")
        .env("ARDUR_DATA_DIR", data_dir.path())
        .env("ARDUR_BIND_ADDR", "127.0.0.1:0")
        .output()
        .expect("the server process runs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "invalid selector must fail, got {:?}",
        output.status
    );
    assert!(
        stderr.contains("building provider: invalid provider selection"),
        "stderr should contain a clean provider-selection error, got: {stderr}"
    );
    assert!(
        stderr.contains("supported values are") && stderr.contains("openai-compat"),
        "stderr should list supported provider values, got: {stderr}"
    );
    assert!(
        !stdout.contains("ardur-server booted") && !stderr.contains("ardur-server booted"),
        "invalid selector should not boot the server, got stdout: {stdout}, stderr: {stderr}"
    );
}
