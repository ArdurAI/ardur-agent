//! Integration tests for `ardur schedule`.

use assert_cmd::Command;

#[test]
fn schedule_create_list_next_delete_lifecycle() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Create a daily schedule.
    let create = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args([
            "schedule",
            "create",
            "daily-summary",
            "daily at 9am",
            "--prompt",
            "summarize my day",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let create_stdout = String::from_utf8(create).expect("stdout utf8");
    assert!(
        create_stdout.contains("created schedule"),
        "{create_stdout}"
    );

    let id = create_stdout
        .lines()
        .find(|l| l.starts_with("created schedule"))
        .and_then(|l| l.split_whitespace().nth(2))
        .expect("schedule id in stdout")
        .to_string();

    // List should contain the schedule.
    let list = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["schedule", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_stdout = String::from_utf8(list).expect("stdout utf8");
    assert!(list_stdout.contains("daily-summary"), "{list_stdout}");

    // Compute next fire times.
    let next = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["schedule", "next", &id, "--count", "3"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let next_stdout = String::from_utf8(next).expect("stdout utf8");
    assert!(next_stdout.contains("next fire times"), "{next_stdout}");

    // Fire dry-run.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["schedule", "fire", &id])
        .assert()
        .success();

    // Delete.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["schedule", "delete", &id])
        .assert()
        .success();

    let schedule_path = dir
        .path()
        .join(".ardur")
        .join("schedules")
        .join(format!("{id}.json"));
    assert!(!schedule_path.exists(), "schedule file should be deleted");
}
