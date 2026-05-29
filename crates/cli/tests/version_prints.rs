//! §2.1 Phase 1: `ardur version` exits 0 and prints the crate version.

use assert_cmd::Command;

#[test]
fn version_prints() {
    // `assert_cmd` treats a `String` stdout predicate as exact-match, so this
    // pins the whole line — both that it exits 0 and that it carries the version.
    let expected = format!("ardur {}\n", env!("CARGO_PKG_VERSION"));
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .arg("version")
        .assert()
        .success()
        .stdout(expected);
}
