//! The shipped binary must use the configured, file-backed assembly path.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn binary_refuses_malformed_revocation_state_before_listening() {
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    std::fs::create_dir_all(root.join("security")).expect("security directory");
    std::fs::write(root.join("security/deny.list"), "not-hex\n").expect("corrupt fixture");
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("ardur-server"));
    command
        .env_clear()
        .env("ARDUR_DATA_DIR", &root)
        .env("ARDUR_BIND_ADDR", "127.0.0.1:0")
        .env("ARDUR_PROVIDER", "anthropic")
        // A synthetic fixture satisfies configuration validation. No provider
        // call can run: malformed revocation state must abort before listening.
        .env("ANTHROPIC_API_KEY", "test-only-no-network")
        .env("ARDUR_CHAT_BEARER_TOKENS", "test-only-chat")
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in ["PATH", "HOME", "TMPDIR", "DYLD_FALLBACK_LIBRARY_PATH"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command.spawn().expect("spawn binary");
    let start = Instant::now();
    loop {
        if child.try_wait().expect("poll binary").is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(20) {
            child.kill().expect("kill binary");
            child.wait().expect("reap binary");
            panic!("binary did not reject corrupt revocation state at startup");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("collect binary");
    assert!(!output.status.success(), "corrupt state must abort boot");
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diagnostics.contains("opening revocation deny list"),
        "startup must fail specifically at revocation-state validation: {diagnostics}"
    );
    assert!(
        !diagnostics.contains("ardur-server booted"),
        "must not reach the listener"
    );
}
