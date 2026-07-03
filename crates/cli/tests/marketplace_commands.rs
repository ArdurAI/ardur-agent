//! Integration tests for `ardur marketplace`.

use assert_cmd::Command;

#[test]
fn marketplace_install_list_show_remove_lifecycle() {
    let dir = tempfile::tempdir().expect("tempdir");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args([
            "marketplace",
            "install",
            "https://example.com/skills/helper.json",
        ])
        .assert()
        .success();

    let list = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_stdout = String::from_utf8(list).expect("stdout utf8");
    assert!(list_stdout.contains("installed-skill"), "{list_stdout}");

    let search = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "search", "installed"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let search_stdout = String::from_utf8(search).expect("stdout utf8");
    assert!(search_stdout.contains("installed-skill"), "{search_stdout}");

    let verify = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "verify"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let verify_stdout = String::from_utf8(verify).expect("stdout utf8");
    assert!(
        verify_stdout.contains("missing or invalid signature"),
        "{verify_stdout}"
    );

    let id = list_stdout
        .lines()
        .find(|l| l.contains("installed-skill"))
        .and_then(|l| l.split_whitespace().next())
        .expect("skill id in list")
        .to_string();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "show", &id])
        .assert()
        .success();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["marketplace", "remove", &id])
        .assert()
        .success();

    let path = dir
        .path()
        .join(".ardur")
        .join("skills")
        .join(format!("{id}.json"));
    assert!(!path.exists(), "skill file should be deleted");
}
