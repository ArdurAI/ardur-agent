//! Integration tests for `ardur channel`.

use assert_cmd::Command;

#[test]
fn channel_lifecycle() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Add a channel.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["channel", "add", "discord", "support-bot"])
        .assert()
        .success();
    let add_output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["channel", "add", "discord", "support-bot"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let add_stdout = String::from_utf8(add_output).expect("stdout utf8");
    assert!(
        add_stdout.contains("added channel support-bot"),
        "{add_stdout}"
    );

    // List should show it as enabled.
    let list = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["channel", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_stdout = String::from_utf8(list).expect("stdout utf8");
    assert!(list_stdout.contains("support-bot"), "{list_stdout}");
    assert!(list_stdout.contains("enabled"), "{list_stdout}");

    // Disable it.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["channel", "set", "support-bot", "disabled"])
        .assert()
        .success();
    let set_output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["channel", "set", "support-bot", "disabled"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let set_stdout = String::from_utf8(set_output).expect("stdout utf8");
    assert!(set_stdout.contains("is now disabled"), "{set_stdout}");

    // Show should print JSON.
    let show = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["channel", "show", "support-bot"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let show_stdout = String::from_utf8(show).expect("stdout utf8");
    assert!(show_stdout.contains("\"enabled\": false"), "{show_stdout}");

    // Remove it.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["channel", "remove", "support-bot"])
        .assert()
        .success();

    let path = dir
        .path()
        .join(".ardur")
        .join("channels")
        .join("support-bot.json");
    assert!(!path.exists(), "channel file should be deleted");
}
