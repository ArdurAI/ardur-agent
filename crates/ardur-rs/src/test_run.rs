use ardur_eval::home::{HomeScenario, run_scenario, unavailable};
use ardur_eval::output::{Format, Summary, render};
use ardur_eval::transcript::Transcript;
use clap::{Parser, Subcommand};
use home_client::{FileStore, HomeClient, SecretStore, default_config_dir};
use serde_json::json;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

// This command's --json takes a file, unlike the device commands' boolean flag.
// Parse its own command tree so their established global flag remains compatible.
#[derive(Parser)]
#[command(name = "ardur-rs test")]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Run a YAML scenario or suite against the paired home, one turn at a time.
    Run {
        input: PathBuf,
        #[arg(long)]
        json: Option<PathBuf>,
        #[arg(long)]
        junit: Option<PathBuf>,
        #[arg(long, default_value = "transcripts")]
        transcripts: PathBuf,
    },
}

fn output(path: Option<PathBuf>) -> Result<Option<File>, &'static str> {
    path.map(|path| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(path)
            .map_err(|_| "Cannot create report; choose a new writable file.")
    })
    .transpose()
}
fn write_report(file: &mut Option<File>, text: &str) -> Result<(), &'static str> {
    if let Some(file) = file {
        file.set_len(0)
            .and_then(|_| file.seek(SeekFrom::Start(0)))
            .map_err(|_| "Cannot update report.")?;
        file.write_all(text.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|_| "Cannot write report.")?;
    }
    Ok(())
}
pub async fn entry(raw: &[OsString]) -> i32 {
    let args = match Args::try_parse_from(raw) {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            print!("{error}");
            return 0;
        }
        Err(_) => {
            eprintln!("Use test run with a scenario file or directory and optional report files.");
            return 2;
        }
    };
    match run(args.command).await {
        Ok(exit) => exit,
        Err(message) => {
            eprintln!("{message}");
            2
        }
    }
}
async fn run(command: Command) -> Result<i32, &'static str> {
    let Command::Run {
        input,
        json,
        junit,
        transcripts,
    } = command;
    let cases = HomeScenario::load(&input)?;
    let client = default_config_dir()
        .and_then(|dir| FileStore::new(dir).load())
        .and_then(HomeClient::new)
        .map_err(|_| "Paired home unavailable; check pairing and private storage.")?;
    let mut json_file = output(json)?;
    let mut junit_file = output(junit)?;
    let mut results = Vec::new();
    let mut interrupted = false;
    for case in &cases {
        if interrupted {
            results.push(unavailable(
                case,
                "Not started because the suite was interrupted.",
            ));
            continue;
        }
        let mut transcript =
            Transcript::create(&transcripts).map_err(|_| "Cannot create transcript.")?;
        let result = tokio::select! {
            result = run_scenario(&client, case, &mut transcript) => result.map_err(|_| "Cannot persist transcript; waiting stopped.")?,
            signal = tokio::signal::ctrl_c() => {
                interrupted = true;
                let reason = if signal.is_ok() { "Interrupted; home work was not cancelled. Recover admitted runs from the transcript." } else { "Interrupt handler unavailable; waiting stopped." };
                let result = unavailable(case, reason);
                transcript.append(json!({"kind":"result","result":result})).map_err(|_| "Cannot persist interruption.")?;
                result
            }
        };
        results.push(result);
        // Keep suite reports useful even if a later test is killed abruptly.
        for (file, format) in [
            (&mut json_file, Format::Json),
            (&mut junit_file, Format::Junit),
        ] {
            write_report(file, &render(&results, format))?;
        }
    }
    // Include any cases skipped after an interrupt in the final totals.
    for (file, format) in [
        (&mut json_file, Format::Json),
        (&mut junit_file, Format::Junit),
    ] {
        write_report(file, &render(&results, format))?;
    }
    println!("{}", render(&results, Format::Markdown));
    let summary = Summary::of(&results);
    Ok(if interrupted {
        130
    } else if results.iter().any(|result| result.home_update_required) {
        5
    } else if summary.failed > 0 {
        1
    } else if summary.unavailable > 0 || summary.errored > 0 {
        2
    } else {
        0
    })
}

#[cfg(test)]
pub(super) fn help_command() -> clap::Command {
    use clap::CommandFactory;
    Args::command()
}
#[cfg(test)]
pub(super) fn help_error(raw: &[String]) -> clap::Error {
    Args::try_parse_from(raw).err().expect("help exits parsing")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_reports_take_file_arguments_instead_of_the_device_json_flag() {
        let args = Args::try_parse_from([
            "test",
            "run",
            "case.yaml",
            "--json",
            "report.json",
            "--junit",
            "report.xml",
            "--transcripts",
            "evidence",
        ])
        .unwrap();
        let Command::Run {
            input,
            json,
            junit,
            transcripts,
        } = args.command;
        assert_eq!(input, PathBuf::from("case.yaml"));
        assert_eq!(json, Some(PathBuf::from("report.json")));
        assert_eq!(junit, Some(PathBuf::from("report.xml")));
        assert_eq!(transcripts, PathBuf::from("evidence"));
        assert!(Args::try_parse_from(["test", "run", "case.yaml", "--json"]).is_err());
    }
}
