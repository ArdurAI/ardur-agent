use std::path::Path;
use std::process::Command;

fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ardur-rs"));
    command.env("NO_COLOR", "1");
    command
}

#[test]
fn binary_help_matches_every_parser_snapshot() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/help");
    for entry in std::fs::read_dir(directory).unwrap() {
        let file = entry.unwrap().path();
        let stem = file.file_stem().unwrap().to_str().unwrap();
        let mut command = binary();
        if stem != "top" {
            command.args(stem.split('-'));
        }
        let output = command.arg("--help").output().unwrap();
        assert!(output.status.success(), "{stem}");
        assert!(output.stderr.is_empty(), "{stem}");
        assert_eq!(
            String::from_utf8(output.stdout)
                .unwrap()
                .replace("\r\n", "\n"),
            std::fs::read_to_string(&file).unwrap(),
            "{stem}: review and update with UPDATE_SNAPSHOTS=1 cargo test --offline --locked -p ardur-rs --bin ardur-rs command_help_snapshots"
        );
    }
}

#[test]
fn binary_version_reports_the_checked_contract_without_pairing() {
    let revision = &home_protocol::COMPATIBILITY.contract_revision;
    let output = binary().arg("--version").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!(
            "ardur-rs {} (home contract {revision})",
            env!("CARGO_PKG_VERSION")
        )
    );
    let output = binary().args(["--version", "--json"]).output().unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["data"]["contractRevision"], *revision);
}
