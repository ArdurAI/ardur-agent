//! Snapshot the actual help parser for every declared command, without a home.
use clap::{Command, CommandFactory, Parser};
use std::collections::BTreeSet;
use std::path::Path;

fn paths(command: &Command, prefix: Vec<String>, result: &mut Vec<Vec<String>>) {
    result.push(prefix.clone());
    for sub in command
        .get_subcommands()
        .filter(|sub| sub.get_name() != "help")
    {
        let mut next = prefix.clone();
        next.push(sub.get_name().to_owned());
        paths(sub, next, result);
    }
}

#[test]
fn command_help_snapshots() {
    let mut commands = Vec::new();
    paths(&super::Args::command(), Vec::new(), &mut commands);
    commands.retain(|path| path.first().is_none_or(|name| name != "test"));
    paths(
        &super::test_run::help_command(),
        vec!["test".into()],
        &mut commands,
    );
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/help");
    let update = std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1");
    let instruction = "Review help changes, then run UPDATE_SNAPSHOTS=1 cargo test --offline --locked -p ardur-rs --bin ardur-rs command_help_snapshots";
    let mut expected = BTreeSet::new();
    for path in commands {
        let filename = if path.is_empty() {
            "top.txt".into()
        } else {
            format!("{}.txt", path.join("-"))
        };
        expected.insert(filename.clone());
        let raw: Vec<_> = std::iter::once("ardur-rs".to_owned())
            .chain(path.clone())
            .chain(["--help".into()])
            .collect();
        // Match main's separate scenario parser, including its different --json.
        let error = if path.first().is_some_and(|p| p == "test") {
            super::test_run::help_error(&raw[1..])
        } else {
            super::Args::try_parse_from(&raw)
                .err()
                .expect("help exits parsing")
        };
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = error.to_string().replace("\r\n", "\n");
        assert!(!help.contains('\r'));
        let file = directory.join(&filename);
        if update {
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(&file, &help).unwrap();
        } else {
            let saved = std::fs::read_to_string(&file)
                .unwrap_or_else(|_| panic!("Missing {filename}. {instruction}"));
            assert_eq!(saved, help, "Help drift in {filename}. {instruction}");
        }
    }
    let actual: BTreeSet<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        actual, expected,
        "Removed commands left stale snapshots; remove them after review. {instruction}"
    );
}
