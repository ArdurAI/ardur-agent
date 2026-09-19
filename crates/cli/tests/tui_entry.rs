//! Environment opt-in must validate before opening persistent state.
use assert_cmd::Command;

#[test]
fn tui_requires_tty_before_creating_state() {
    let home = tempfile::tempdir().unwrap();
    let result = Command::cargo_bin("ardur")
        .unwrap()
        .args(["chat"])
        .env("HOME", home.path())
        .env("ARDUR_TUI", "1")
        .write_stdin("/quit\n")
        .output()
        .unwrap();
    assert!(
        !result.status.success(),
        "full-screen mode must reject piped input"
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("interactive stdin and stdout"));
    assert!(
        !home.path().join(".ardur").exists(),
        "validation must precede state creation"
    );
}

#[test]
fn tui_rejects_conflicting_modes_and_invalid_opt_in() {
    for (value, flag, label) in [
        ("1", "--echo", "--echo"),
        ("1", "--plain", "--plain"),
        ("1", "--no-stream", "--no-stream"),
        ("yes", "--echo", "ARDUR_TUI"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let result = Command::cargo_bin("ardur")
            .unwrap()
            .args(["chat", flag])
            .env("HOME", home.path())
            .env("ARDUR_TUI", value)
            .write_stdin("/quit\n")
            .output()
            .unwrap();
        assert!(!result.status.success(), "must reject {value} {flag}");
        assert!(String::from_utf8_lossy(&result.stderr).contains(label));
        assert!(!home.path().join(".ardur").exists());
    }
}

#[test]
fn disabled_tui_preserves_echo_repl() {
    for enabled in [None, Some("0")] {
        let home = tempfile::tempdir().unwrap();
        let mut cmd = Command::cargo_bin("ardur").unwrap();
        cmd.args(["chat", "--echo"])
            .env("HOME", home.path())
            .env_remove("ARDUR_TUI");
        if let Some(value) = enabled {
            cmd.env("ARDUR_TUI", value);
        }
        let result = cmd.write_stdin("hello\n/quit\n").output().unwrap();
        assert!(result.status.success());
        assert!(String::from_utf8_lossy(&result.stdout).contains("hello"));
    }
}
