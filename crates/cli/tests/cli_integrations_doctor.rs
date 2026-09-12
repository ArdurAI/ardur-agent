//! ARD-459 — `ardur doctor` reports declared integrations.
//!
//! Doctor is the operator's answer to "is this integration actually going to
//! work?". These tests drive the real binary, so they prove the reporting
//! reaches the JSON an operator sees rather than merely that a helper returns
//! the right struct.

use assert_cmd::Command;

/// An `ardur` invocation hermetic from the ambient shell.
///
/// `ARDUR_INTEGRATIONS_*` is cleared explicitly: a developer with an override
/// exported would otherwise change what these tests observe.
fn ardur(home: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("ardur").expect("the `ardur` binary builds");
    cmd.env("HOME", home)
        .env_remove("ARDUR_DATA_DIR")
        .env_remove("ARDUR_INTEGRATIONS_BEADS_ENABLED")
        .env_remove("ARDUR_INTEGRATIONS_BEADS_COMMAND")
        .env_remove("ARDUR_INTEGRATIONS_OBSIDIAN_ENABLED")
        .env_remove("ARDUR_INTEGRATIONS_OBSIDIAN_ROOT");
    cmd
}

/// Run `ardur doctor --json` and parse the report.
///
/// Doctor exits non-zero when it finds hard failures, which is expected on a
/// bare temp HOME, so the exit status is ignored and only the report is read.
fn doctor_report(home: &std::path::Path) -> serde_json::Value {
    let output = ardur(home).args(["doctor"]).output().expect("doctor runs");
    let stdout = String::from_utf8(output.stdout).expect("doctor emits UTF-8");
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("doctor emits JSON ({e}): {stdout}"))
}

/// Find a named check in a doctor report.
fn check<'a>(report: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    report
        .get("checks")?
        .as_array()?
        .iter()
        .find(|c| c.get("name").and_then(|n| n.as_str()) == Some(name))
}

#[test]
fn a_fresh_install_declares_no_integrations() {
    // The acceptance criterion for the default posture, observed through the
    // operator-facing surface rather than asserted about internals.
    let home = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(home.path().join(".ardur")).expect("state dir");
    std::fs::write(home.path().join(".ardur").join("config.toml"), "")
        .expect("an empty config file");

    let report = doctor_report(home.path());
    let integrations = check(&report, "integrations").expect("doctor reports integrations");

    assert_eq!(integrations["declared"], 0);
    assert_eq!(integrations["enabled"], 0);
    assert_eq!(integrations["status"], "ok");
}

#[test]
fn a_declared_integration_is_reported_with_its_enabled_state() {
    let home = tempfile::tempdir().expect("tempdir");
    let state = home.path().join(".ardur");
    std::fs::create_dir_all(&state).expect("state dir");

    // A vault that exists, declared but left OFF.
    let vault = home.path().join("vault");
    std::fs::create_dir_all(&vault).expect("vault");
    std::fs::write(
        state.join("config.toml"),
        format!("[integrations.obsidian]\nroot = \"{}\"\n", vault.display()),
    )
    .expect("config");

    let report = doctor_report(home.path());

    let summary = check(&report, "integrations").expect("summary present");
    assert_eq!(summary["declared"], 1);
    assert_eq!(
        summary["enabled"], 0,
        "declaring an integration must not enable it"
    );

    let entry = check(&report, "integration:obsidian").expect("per-integration check present");
    assert_eq!(entry["enabled"], false);
    assert_eq!(entry["present"], true, "the vault directory exists");
    assert_eq!(entry["kind"], "directory");
}

#[test]
fn an_enabled_integration_with_a_missing_resource_warns() {
    // The case doctor exists to catch: configuration says the integration is
    // on, but it cannot possibly work on this host.
    let home = tempfile::tempdir().expect("tempdir");
    let state = home.path().join(".ardur");
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::write(
        state.join("config.toml"),
        "[integrations.obsidian]\nroot = \"/nonexistent/vault/path\"\nenabled = true\n",
    )
    .expect("config");

    let report = doctor_report(home.path());
    let entry = check(&report, "integration:obsidian").expect("per-integration check present");

    assert_eq!(entry["enabled"], true);
    assert_eq!(entry["present"], false);
    assert_eq!(
        entry["status"], "warn",
        "an enabled integration whose resource is missing must warn"
    );
}

#[test]
fn a_disabled_integration_with_a_missing_resource_does_not_warn() {
    // The contrast that makes the previous test meaningful: the same missing
    // resource is not a problem when the integration is off, so an operator
    // can share one config file across machines.
    let home = tempfile::tempdir().expect("tempdir");
    let state = home.path().join(".ardur");
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::write(
        state.join("config.toml"),
        "[integrations.obsidian]\nroot = \"/nonexistent/vault/path\"\n",
    )
    .expect("config");

    let report = doctor_report(home.path());
    let entry = check(&report, "integration:obsidian").expect("per-integration check present");

    assert_eq!(entry["present"], false);
    assert_eq!(
        entry["status"], "skipped",
        "a disabled integration must not warn about a missing resource"
    );
}

#[test]
fn malformed_integration_configuration_is_reported_rather_than_ignored() {
    // An unknown key fails the parse. Doctor must surface that, because the
    // alternative is an operator who believes a setting took effect.
    let home = tempfile::tempdir().expect("tempdir");
    let state = home.path().join(".ardur");
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::write(
        state.join("config.toml"),
        "[integrations.beads]\ncommand = \"bd\"\nenable = true\n",
    )
    .expect("config");

    let report = doctor_report(home.path());
    let entry = check(&report, "integrations").expect("integrations check present");

    assert_eq!(entry["status"], "warn");
    let note = entry["note"].as_str().expect("a note explains the failure");
    assert!(
        note.contains("enable"),
        "the note must name the offending key so it can be fixed; got: {note}"
    );
}

/// The documented `[integrations.<name>]` config must not break the CLI.
///
/// `Config::load` routes through a hand-rolled flat-TOML reader. Before this
/// was fixed, a table header had no `=` and so failed the parse: the exact
/// configuration documented in RUN.md made every config-loading CLI command
/// error out. Documenting a config that breaks the tool is worse than not
/// supporting it.
#[test]
fn the_documented_integration_config_does_not_break_the_cli_config_loader() {
    let home = tempfile::tempdir().expect("tempdir");
    let state = home.path().join(".ardur");
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::write(
        state.join("config.toml"),
        "model = \"a-model\"\n\
         budget_cents = 250\n\
         \n\
         [integrations.obsidian]\n\
         root = \"/tmp/vault\"\n\
         enabled = true\n",
    )
    .expect("config");

    let output = ardur(home.path())
        .args(["config"])
        .output()
        .expect("config runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "`ardur config` must load a file containing the documented integrations \
         section; stdout={stdout} stderr={stderr}"
    );
    // The root-level keys still apply...
    assert!(
        stdout.contains("a-model"),
        "root-level keys must still be read; got: {stdout}"
    );
    // ...and the table's keys did NOT leak into the root namespace.
    assert!(
        !stdout.contains("/tmp/vault"),
        "keys inside [integrations.*] must not be applied as root config; got: {stdout}"
    );
}

/// Doctor's output is advertised as safe to paste into an issue.
///
/// `toml`'s error message quotes the offending source line. A malformed
/// assignment on a credential line therefore puts the credential into the
/// report unless it is redacted. Verified against `toml` 1.1.2, which does
/// include the raw value for an unterminated string.
#[test]
fn a_malformed_config_does_not_leak_secrets_into_the_doctor_report() {
    let home = tempfile::tempdir().expect("tempdir");
    let state = home.path().join(".ardur");
    std::fs::create_dir_all(&state).expect("state dir");
    // An unterminated string on the api_key line: the parse fails *there*, so
    // the parser's excerpt is of the credential line.
    std::fs::write(
        state.join("config.toml"),
        "api_key = \"sk-live-NOTAREALSECRET-abc123\n[integrations.beads]\ncommand = \"bd\"\n",
    )
    .expect("config");

    let output = ardur(home.path())
        .args(["doctor"])
        .output()
        .expect("doctor runs");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !stdout.contains("NOTAREALSECRET"),
        "the doctor report must not echo a credential from the config file; got: {stdout}"
    );
    assert!(
        stdout.contains("could not be parsed"),
        "the failure must still be reported, just without the excerpt; got: {stdout}"
    );
}

/// The beads adapter must be reachable from the shipped binary.
///
/// Adding a workspace dependency does not make an adapter available: the
/// production doctor path built an empty `AdapterRegistry`, so a correctly
/// configured beads integration was reported as having no adapter and would
/// have failed the boot. This asserts the opposite through the real binary.
#[test]
fn the_beads_adapter_is_wired_into_the_shipped_binary() {
    let home = tempfile::tempdir().expect("tempdir");
    let state = home.path().join(".ardur");
    std::fs::create_dir_all(&state).expect("state dir");

    // An executable stub, so "present" is true and the only thing under test
    // is whether an adapter exists for the integration.
    let bd = home.path().join("bd");
    std::fs::write(&bd, "#!/bin/sh\nexit 0\n").expect("stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&bd).expect("metadata").permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&bd, p).expect("chmod");
    }

    std::fs::write(
        state.join("config.toml"),
        format!(
            "[integrations.beads]\ncommand = \"{}\"\nenabled = true\n",
            bd.display()
        ),
    )
    .expect("config");

    let report = doctor_report(home.path());
    let entry = check(&report, "integration:beads").expect("per-integration check present");

    assert_eq!(
        entry["adapter"], true,
        "the binary must carry a beads adapter, or an enabled beads integration \
         cannot boot; got: {entry}"
    );
    assert_eq!(entry["present"], true, "the stub is executable");
    assert_eq!(
        entry["status"], "ok",
        "with an adapter and a present binary, beads is healthy: {entry}"
    );
}

/// An integration with no adapter is still reported as such.
///
/// The contrast that keeps the previous test meaningful: wiring beads in must
/// not make every integration look adapted.
#[test]
fn an_integration_without_an_adapter_is_still_reported_as_unadapted() {
    let home = tempfile::tempdir().expect("tempdir");
    let state = home.path().join(".ardur");
    std::fs::create_dir_all(&state).expect("state dir");
    let vault = home.path().join("vault");
    std::fs::create_dir_all(&vault).expect("vault");

    std::fs::write(
        state.join("config.toml"),
        format!(
            "[integrations.obsidian]\nroot = \"{}\"\nenabled = true\n",
            vault.display()
        ),
    )
    .expect("config");

    let report = doctor_report(home.path());
    let entry = check(&report, "integration:obsidian").expect("check present");

    assert_eq!(
        entry["adapter"], false,
        "obsidian has no adapter in this build: {entry}"
    );
    assert_eq!(
        entry["status"], "warn",
        "an enabled integration with no adapter must warn: {entry}"
    );
}
