//! Integration tests for `ardur nodes` device mesh control plane.

use assert_cmd::Command;
use serde_json::Value;

fn pair_and_approve(home: &std::path::Path, device_id: &str) {
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", home)
        .args([
            "nodes",
            "pair",
            device_id,
            "--platform",
            "macos",
            "--cap",
            "tool.browser.open",
            "--cap",
            "tool.shell.readonly",
            "--trust-tier",
            "personal",
            "--ttl-seconds",
            "3600",
        ])
        .assert()
        .success();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", home)
        .args(["nodes", "approve", device_id])
        .assert()
        .success();
}

#[test]
fn nodes_pair_approve_route_and_revoke_with_receipt() {
    let dir = tempfile::tempdir().expect("tempdir");
    pair_and_approve(dir.path(), "macbook-pro");

    let receipt_path = dir.path().join("route-receipt.json");
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .arg("nodes")
        .arg("route-tool")
        .arg("macbook-pro")
        .arg("browser.open")
        .arg("--capability")
        .arg("tool.browser.open")
        .arg("--receipt")
        .arg(&receipt_path)
        .assert()
        .success();

    let receipt: Value =
        serde_json::from_slice(&std::fs::read(&receipt_path).expect("receipt file"))
            .expect("receipt json");
    assert_eq!(receipt["device_id"], "macbook-pro");
    assert_eq!(receipt["status"], "routed");
    assert_eq!(receipt["capability"], "tool.browser.open");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["nodes", "revoke", "macbook-pro"])
        .assert()
        .success();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .arg("nodes")
        .arg("route-tool")
        .arg("macbook-pro")
        .arg("browser.open")
        .arg("--capability")
        .arg("tool.browser.open")
        .arg("--receipt")
        .arg(dir.path().join("after-revoke.json"))
        .assert()
        .failure();
}

#[test]
fn nodes_fail_closed_for_unapproved_missing_capability_and_emergency_stop() {
    let dir = tempfile::tempdir().expect("tempdir");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args([
            "nodes",
            "pair",
            "iphone",
            "--platform",
            "ios",
            "--cap",
            "tool.camera.capture",
        ])
        .assert()
        .success();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .arg("nodes")
        .arg("route-tool")
        .arg("iphone")
        .arg("camera.capture")
        .arg("--capability")
        .arg("tool.camera.capture")
        .arg("--receipt")
        .arg(dir.path().join("pending.json"))
        .assert()
        .failure();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["nodes", "approve", "iphone"])
        .assert()
        .success();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .arg("nodes")
        .arg("route-tool")
        .arg("iphone")
        .arg("shell.exec")
        .arg("--capability")
        .arg("tool.shell.exec")
        .arg("--receipt")
        .arg(dir.path().join("denied-capability.json"))
        .assert()
        .failure();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["nodes", "emergency-stop", "--enable"])
        .assert()
        .success();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .arg("nodes")
        .arg("route-tool")
        .arg("iphone")
        .arg("camera.capture")
        .arg("--capability")
        .arg("tool.camera.capture")
        .arg("--receipt")
        .arg(dir.path().join("stopped.json"))
        .assert()
        .failure();
}

#[test]
fn nodes_support_stale_offline_fallback_and_status_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    pair_and_approve(dir.path(), "android-tablet");

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .arg("nodes")
        .arg("route-tool")
        .arg("android-tablet")
        .arg("browser.open")
        .arg("--capability")
        .arg("tool.browser.open")
        .arg("--receipt")
        .arg(dir.path().join("stale-denied.json"))
        .arg("--stale-after-secs")
        .arg("0")
        .assert()
        .failure();

    let fallback = dir.path().join("offline-fallback.json");
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .arg("nodes")
        .arg("route-tool")
        .arg("android-tablet")
        .arg("browser.open")
        .arg("--capability")
        .arg("tool.browser.open")
        .arg("--receipt")
        .arg(&fallback)
        .arg("--stale-after-secs")
        .arg("0")
        .arg("--offline-ok")
        .assert()
        .success();

    let receipt: Value = serde_json::from_slice(&std::fs::read(&fallback).expect("receipt file"))
        .expect("receipt json");
    assert_eq!(receipt["status"], "offline-fallback");

    let status = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["nodes", "status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&status).expect("status json");
    assert_eq!(status["devices"][0]["id"], "android-tablet");
    assert_eq!(status["sessions"].as_array().expect("sessions").len(), 1);
    assert_eq!(status["receipts"].as_array().expect("receipts").len(), 1);
}
