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

    // Fire the schedule end-to-end (issue #347). With no provider credentials
    // the fire runs against the offline stub; `ARDUR_DEV_PERMISSIVE_POLICY`
    // supplies the permit the fused pipeline's Cedar stage requires. A signed
    // receipt must land on the chain — proof the job actually executed rather
    // than printing the old "execution engine not yet wired" stub.
    let fire = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .env("ARDUR_DEV_PERMISSIVE_POLICY", "true")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .args(["schedule", "fire", &id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let fire_stdout = String::from_utf8(fire).expect("stdout utf8");
    assert!(
        fire_stdout.contains("fired schedule") && fire_stdout.contains("receipt:"),
        "{fire_stdout}"
    );
    let chain = dir
        .path()
        .join(".ardur")
        .join("receipts")
        .join("chain.jsonl");
    let chain_lines = std::fs::read_to_string(&chain)
        .expect("the receipt chain exists after a fire")
        .lines()
        .filter(|l| !l.is_empty())
        .count();
    assert_eq!(chain_lines, 1, "the fire minted exactly one receipt");

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
