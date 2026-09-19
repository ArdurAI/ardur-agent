//! Binary activation and terminal-mode proof, distinct from frame goldens.
#[cfg(unix)]
#[test]
fn env_opt_in_enters_full_screen_and_restores_terminal() {
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            include_str!("fixtures/tui/terminal_modes.py"),
            env!("CARGO_BIN_EXE_ardur"),
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home.path().canonicalize().unwrap())
        .env("TERM", "xterm-256color")
        .env("ARDUR_PROVIDER", "anthropic")
        .env("ARDUR_TUI", "1")
        .output()
        .expect("python3 PTY harness");
    assert!(
        output.status.success(),
        "PTY proof failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("terminal modes restored"));
}
