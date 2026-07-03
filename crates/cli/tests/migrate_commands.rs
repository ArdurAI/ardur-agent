//! Integration tests for `ardur migrate`.

use assert_cmd::Command;
use serde_json::json;

#[test]
fn migrate_export_import_roundtrip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let export_dir = dir.path().join("export");

    // Create a dummy memory card in the ardur state.
    let ardur_dir = dir.path().join(".ardur");
    let memory_dir = ardur_dir.join("memory");
    std::fs::create_dir_all(&memory_dir).expect("create memory dir");
    let card = json!({
        "id": "card-1",
        "content": "hello world",
        "created_at": 0,
    });
    std::fs::write(
        memory_dir.join("card-1.json"),
        serde_json::to_string_pretty(&card).expect("serialize"),
    )
    .expect("write card");

    // Export.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["migrate", "export", export_dir.to_str().unwrap()])
        .assert()
        .success();

    let manifest_path = export_dir.join("manifest.json");
    assert!(manifest_path.exists(), "manifest should exist");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).expect("read manifest"))
            .expect("parse manifest");
    assert_eq!(manifest.get("files").and_then(|v| v.as_i64()), Some(1));

    // Import into a fresh ardur state.
    let import_home = dir.path().join("import_home");
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &import_home)
        .args(["migrate", "import", export_dir.to_str().unwrap()])
        .assert()
        .success();

    let imported_card = import_home
        .join(".ardur")
        .join("memory")
        .join("card-1.json");
    assert!(imported_card.exists(), "imported card should exist");
}

#[test]
fn migrate_from_hermes_copies_sessions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hermes_dir = dir.path().join("hermes");
    let hermes_history = hermes_dir.join("history");
    std::fs::create_dir_all(&hermes_history).expect("create hermes history");
    std::fs::write(hermes_history.join("sess-1.json"), r#"{"messages": []}"#)
        .expect("write hermes session");

    let output = dir.path().join("out");
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args([
            "migrate",
            "from-hermes",
            hermes_dir.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(output.join("sessions").join("sess-1.json").exists());
}

#[test]
fn migrate_from_openclaw_copies_sessions_and_memory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let openclaw_dir = dir.path().join("openclaw");
    let oc_sessions = openclaw_dir.join("sessions");
    let oc_memory = openclaw_dir.join("memory");
    std::fs::create_dir_all(&oc_sessions).expect("create oc sessions");
    std::fs::create_dir_all(&oc_memory).expect("create oc memory");
    std::fs::write(oc_sessions.join("s.json"), r#"{"messages": []}"#).expect("write session");
    std::fs::write(oc_memory.join("m.json"), r#"{"note": "x"}"#).expect("write memory");

    let output = dir.path().join("out");
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args([
            "migrate",
            "from-open-claw",
            openclaw_dir.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(output.join("sessions").join("s.json").exists());
    assert!(output.join("memory").join("m.json").exists());
}
