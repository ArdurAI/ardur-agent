//! Integration tests for `ardur persona`.

use assert_cmd::Command;
use serde_json::json;

#[test]
fn persona_create_set_active_remove_lifecycle() {
    let dir = tempfile::tempdir().expect("tempdir");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["persona", "create", "coder"])
        .assert()
        .success();

    // List should show the persona.
    let list = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["persona", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_stdout = String::from_utf8(list).expect("stdout utf8");
    assert!(list_stdout.contains("coder"), "{list_stdout}");

    // Set active.
    let set = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["persona", "set", "coder"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let set_stdout = String::from_utf8(set).expect("stdout utf8");
    assert!(
        set_stdout.contains("active persona set to coder"),
        "{set_stdout}"
    );

    // Show active.
    let active = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["persona", "active"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let active_stdout = String::from_utf8(active).expect("stdout utf8");
    assert!(
        active_stdout.contains("\"is_active\": true"),
        "{active_stdout}"
    );

    // Remove.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["persona", "remove", "coder"])
        .assert()
        .success();

    let path = dir
        .path()
        .join(".ardur")
        .join("personas")
        .join("coder.json");
    assert!(!path.exists(), "persona file should be deleted");
}

#[test]
fn persona_install_pack() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pack_dir = dir.path().join("pack");
    std::fs::create_dir_all(&pack_dir).expect("create pack dir");
    std::fs::write(
        pack_dir.join("helper.json"),
        json!({
            "name": "helper",
            "display_name": "Helper",
            "system_prompt": "You are a helpful assistant.",
            "domains": ["general"],
            "tone": "friendly",
            "is_active": false
        })
        .to_string(),
    )
    .expect("write pack");

    let install = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["persona", "install-pack", pack_dir.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let install_stdout = String::from_utf8(install).expect("stdout utf8");
    assert!(
        install_stdout.contains("installed 1 personas"),
        "{install_stdout}"
    );

    let path = dir
        .path()
        .join(".ardur")
        .join("personas")
        .join("helper.json");
    assert!(path.exists(), "pack persona should be installed");
}
