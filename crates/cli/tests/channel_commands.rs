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

/// The variables `channel add` prints must be the ones the SERVER requires.
///
/// This is the gh#521 regression: the CLI printed a per-channel prefix
/// (`TELEGRAM_SUPPORT_BOT`) that nothing ever read, so an operator who followed
/// the instruction hit "TELEGRAM_BOT_TOKEN is unset" immediately after setting
/// a token. The names are asserted against `crates/server/src/config.rs`
/// itself, so the CLI cannot drift away from the server's requirements without
/// this failing.
#[test]
fn printed_activation_variables_are_the_ones_the_server_requires() {
    let server_config = include_str!("../../server/src/config.rs");

    // (channel type, variables the CLI must print)
    let expected: &[(&str, &[&str])] = &[
        (
            "telegram",
            &["ARDUR_CHANNEL_TELEGRAM=1", "TELEGRAM_BOT_TOKEN"],
        ),
        (
            "discord",
            &[
                "ARDUR_CHANNEL_DISCORD=1",
                "DISCORD_BOT_TOKEN",
                "DISCORD_APPLICATION_ID",
            ],
        ),
        (
            "matrix",
            &[
                "ARDUR_CHANNEL_MATRIX=1",
                "MATRIX_HOMESERVER_URL",
                "MATRIX_USER_ID",
                "MATRIX_ACCESS_TOKEN",
            ],
        ),
    ];

    for (channel_type, vars) in expected {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = Command::cargo_bin("ardur")
            .expect("the `ardur` binary builds")
            .env("HOME", dir.path())
            .args(["channel", "add", channel_type, "support-bot"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let stdout = String::from_utf8(output).expect("stdout utf8");

        for var in *vars {
            // The CLI must print it...
            assert!(
                stdout.contains(var),
                "`channel add {channel_type}` must print {var}, got:\n{stdout}"
            );
            // ...and the server must actually read it, or the instruction is
            // fiction again in a new spelling.
            let name = var.split('=').next().expect("non-empty");
            assert!(
                server_config.contains(&format!("\"{name}\"")),
                "the CLI prints {name}, but the server config never mentions it"
            );
        }

        // The old per-channel prefix must not reappear as an instruction.
        assert!(
            !stdout.contains("env prefix"),
            "`channel add` must not advertise a prefix nothing reads, got:\n{stdout}"
        );
        let shouty = format!("{}_SUPPORT_BOT", channel_type.to_uppercase());
        assert!(
            !stdout.contains(&shouty),
            "`channel add` must not print the derived prefix {shouty}, got:\n{stdout}"
        );
    }
}

/// Every channel type the CLI ACCEPTS must have known activation variables.
///
/// `channel_type` is constrained by clap to a fixed list, so an unrecognised
/// value is rejected before our code runs. The live risk is the reverse: a type
/// clap accepts but the activation table does not cover would print "unknown
/// channel type" for a perfectly valid channel. This reads the accepted list
/// out of the source so adding a fifth channel fails here until it is mapped.
#[test]
fn every_accepted_channel_type_has_activation_variables() {
    let main_rs = include_str!("../src/main.rs");
    let marker = "#[arg(value_parser = [";
    let start = main_rs.find(marker).expect("the channel value_parser list");
    let list = &main_rs[start + marker.len()..];
    let list = &list[..list.find("])").expect("a closed value_parser list")];
    let accepted: Vec<String> = list
        .split(',')
        .map(|entry| entry.trim().trim_matches('"').to_string())
        .filter(|entry| !entry.is_empty())
        .collect();

    assert!(
        accepted.len() >= 4,
        "expected the four known channel types, parsed {accepted:?}"
    );

    for channel_type in &accepted {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = Command::cargo_bin("ardur")
            .expect("the `ardur` binary builds")
            .env("HOME", dir.path())
            .args(["channel", "add", channel_type, "support-bot"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let stdout = String::from_utf8(output).expect("stdout utf8");

        assert!(
            !stdout.contains("unknown channel type"),
            "`{channel_type}` is accepted by the CLI but has no activation \
             variables mapped, got:\n{stdout}"
        );
        assert!(
            stdout.contains("to activate, set:"),
            "`{channel_type}` must print activation instructions, got:\n{stdout}"
        );
    }
}
