use clap::{Parser, Subcommand};
use home_client::{
    CommandResult, DeviceCommand, Error, FileStore, HomeClient, SecretStore, default_config_dir,
    execute_device, execute_room_send, human_text, pair_device, redact,
};
use serde_json::{Value, json};
use std::io::Read;
use zeroize::Zeroizing;
#[derive(Parser)]
#[command(name = "ardur-rs", version, about = "Paired Ardur home client")]
struct Args {
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Pair {
        #[arg(long)]
        file: String,
        #[arg(long, default_value = "Command line")]
        name: String,
    },
    Status,
    Send {
        bot: String,
        text: String,
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        wait: bool,
        #[arg(long, requires = "wait", value_parser = duration)]
        timeout: Option<std::time::Duration>,
    },
    Wait {
        #[arg(long)]
        run: String,
        #[arg(long, default_value = "180s", value_parser = duration)]
        timeout: std::time::Duration,
    },
    Runs {
        #[command(subcommand)]
        command: RunsCommand,
    },
    Tasks {
        #[command(subcommand)]
        command: TasksCommand,
    },
    Stop {
        task_id: String,
    },
    Bots {
        #[command(subcommand)]
        command: BotsCommand,
    },
    Computers {
        #[command(subcommand)]
        command: ComputersCommand,
    },
    Board {
        #[command(subcommand)]
        command: BoardCommand,
    },
    Rooms {
        #[command(subcommand)]
        command: RoomsCommand,
    },
}
#[derive(Subcommand)]
enum BotsCommand {
    List,
}
#[derive(Subcommand)]
enum RunsCommand {
    List {
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    Show {
        run_id: String,
    },
}
#[derive(Subcommand)]
enum ComputersCommand {
    List,
}
#[derive(Subcommand)]
enum BoardCommand {
    List {
        #[arg(long)]
        workspace: String,
        /// Board filter as a JSON object, for example {"status":"open"}.
        #[arg(long, value_parser = board_filter)]
        filter: Option<serde_json::Value>,
        #[arg(long)]
        search: Option<String>,
    },
    Show {
        #[arg(long)]
        workspace: String,
        item: String,
    },
}
#[derive(Subcommand)]
enum RoomsCommand {
    List,
    Send {
        #[arg(long, conflicts_with = "room_id")]
        room: Option<String>,
        #[arg(long)]
        room_id: Option<String>,
        #[arg(long)]
        thread: Option<String>,
        text: String,
    },
}
#[derive(Subcommand)]
enum TasksCommand {
    Show { task_id: String },
}
fn board_filter(raw: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| {
        "Choose a board filter as a JSON object, for example {\"status\":\"open\"}.".to_owned()
    })?;
    if value.is_object() {
        Ok(value)
    } else {
        Err("Choose a board filter as a JSON object, for example {\"status\":\"open\"}.".into())
    }
}
fn duration(raw: &str) -> Result<std::time::Duration, String> {
    let (number, scale) = if let Some(n) = raw.strip_suffix("ms") {
        (n, 1.0)
    } else if let Some(n) = raw.strip_suffix('s') {
        (n, 1000.0)
    } else if let Some(n) = raw.strip_suffix('m') {
        (n, 60000.0)
    } else {
        (raw, 1000.0)
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let mut parts = number.split('.');
    let valid = parts.next().is_some_and(digits)
        && parts.next().is_none_or(digits)
        && parts.next().is_none();
    if !valid {
        return Err("Choose a positive timeout.".into());
    }
    let millis = number
        .parse::<f64>()
        .map_err(|_| "Choose a positive timeout.")?
        * scale;
    if !millis.is_finite() || millis.fract() != 0.0 || !(1.0..=2147483647.0).contains(&millis) {
        return Err("Choose a positive timeout.".into());
    }
    Ok(std::time::Duration::from_millis(millis as u64))
}
fn device_command(command: Command) -> Result<(DeviceCommand, std::time::Duration), Command> {
    let default = std::time::Duration::from_secs(180);
    Ok(match command {
        Command::Send {
            bot,
            text,
            request_id,
            wait,
            timeout,
        } => (
            DeviceCommand::Send {
                bot,
                text,
                request_id,
                wait,
            },
            timeout.unwrap_or(default),
        ),
        Command::Wait { run, timeout } => (DeviceCommand::Wait { run_id: run }, timeout),
        Command::Runs {
            command: RunsCommand::List { cursor, limit },
        } => (DeviceCommand::RunsList { cursor, limit }, default),
        Command::Runs {
            command: RunsCommand::Show { run_id },
        } => (DeviceCommand::RunsShow { run_id }, default),
        Command::Tasks {
            command: TasksCommand::Show { task_id },
        } => (DeviceCommand::TasksShow { task_id }, default),
        Command::Stop { task_id } => (DeviceCommand::Stop { task_id }, default),
        Command::Computers {
            command: ComputersCommand::List,
        } => (DeviceCommand::ComputersList, default),
        Command::Board {
            command:
                BoardCommand::List {
                    workspace,
                    filter,
                    search,
                },
        } => (
            DeviceCommand::BoardList {
                workspace,
                filter,
                search,
            },
            default,
        ),
        Command::Board {
            command: BoardCommand::Show { workspace, item },
        } => (DeviceCommand::BoardShow { workspace, item }, default),
        Command::Rooms {
            command: RoomsCommand::List,
        } => (DeviceCommand::RoomsList, default),
        // Room sends recover through the durable pending record in the store.
        other => return Err(other),
    })
}
fn print_device(result: CommandResult, json_mode: bool) {
    if json_mode {
        println!("{}", result.json());
    } else {
        println!("{}", human_text(&result.human()));
    }
}
fn input(file: &str) -> Result<Zeroizing<String>, Error> {
    let mut bytes = Zeroizing::new(Vec::new());
    if file == "-" {
        std::io::stdin()
            .take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Input)?;
    } else {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let f = options.open(file).map_err(|_| Error::Input)?;
        if !f.metadata().map_err(|_| Error::Input)?.is_file() {
            return Err(Error::Input);
        }
        f.take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Input)?;
    }
    if bytes.len() > 65536 {
        return Err(Error::Input);
    }
    Ok(Zeroizing::new(
        std::str::from_utf8(&bytes)
            .map_err(|_| Error::Input)?
            .to_owned(),
    ))
}
async fn execute(command: Command) -> Result<Value, Error> {
    let store = FileStore::new(default_config_dir()?);
    match command {
        Command::Pair { file, name } => {
            store.prepare()?;
            let code = input(&file)?;
            let home = pair_device(&code, &name).await?;
            store.save(&home)?;
            Ok(
                json!({"homeName":home.profile.home_name,"instanceId":home.profile.pins.instance_id,"paired":true}),
            )
        }
        Command::Status => HomeClient::new(store.load()?)?.status().await,
        Command::Bots {
            command: BotsCommand::List,
        } => HomeClient::new(store.load()?)?.bots().await,
        _ => Err(Error::Input),
    }
}
fn report(command: &str, json_mode: bool, result: Result<Value, Error>) -> i32 {
    match result {
        Ok(data) => {
            let data = redact(data);
            if json_mode {
                println!(
                    "{}",
                    json!({"schemaVersion":1,"ok":true,"command":command,"data":data})
                )
            } else {
                match command {
                    "pair" => println!(
                        "Paired with {}.",
                        human_text(data["homeName"].as_str().unwrap_or(""))
                    ),
                    "status" => println!(
                        "Paired with {}. Device is valid.",
                        human_text(data["homeName"].as_str().unwrap_or(""))
                    ),
                    "bots list" => {
                        for bot in data["bots"].as_array().into_iter().flatten() {
                            println!(
                                "{}\t{}",
                                human_text(bot["id"].as_str().unwrap_or("")),
                                human_text(bot["name"].as_str().unwrap_or(""))
                            );
                        }
                    }
                    _ => {}
                }
            }
            0
        }
        Err(e) => {
            if json_mode {
                println!(
                    "{}",
                    json!({"schemaVersion":1,"ok":false,"command":command,"error":{"code":e.code(),"message":e.to_string()},"exitCode":e.exit_code()})
                )
            } else {
                eprintln!("{e}")
            }
            e.exit_code()
        }
    }
}
#[tokio::main]
async fn main() {
    let raw = std::env::args_os().collect::<Vec<_>>();
    let json_mode = raw.iter().any(|a| a == "--json");
    let args = match Args::try_parse_from(&raw) {
        Ok(a) => a,
        Err(e) => {
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                if json_mode {
                    println!(
                        "{}",
                        json!({"schemaVersion":1,"ok":true,"command":"help","data":{"usage":"ardur-rs [--json] pair --file <path|-> [--name <name>] | status | bots list | send <bot> <text> --request-id <id> [--wait] [--timeout 180s] | wait --run <id> [--timeout 180s] | runs list [--cursor <id>] [--limit 50] | runs show <id> | tasks show <id> | stop <task-id> | computers list | board list --workspace <id> [--filter <json>] [--search <text>] | board show --workspace <id> <item> | rooms list | rooms send (--room <name> | --room-id <id>) [--thread <id>] <text>","version":env!("CARGO_PKG_VERSION")}})
                    )
                } else {
                    print!("{e}")
                }
                return;
            }
            // Do not echo invalid arguments (they may contain the pairing challenge).
            const WORDS: [&str; 8] = [
                "send",
                "wait",
                "runs",
                "tasks",
                "stop",
                "computers",
                "board",
                "rooms",
            ];
            let name = raw
                .iter()
                .filter_map(|a| a.to_str())
                .find(|a| WORDS.contains(a))
                .unwrap_or("unknown");
            if WORDS.contains(&name) || raw.iter().any(|a| WORDS.iter().any(|w| a == w)) {
                let (result, exit) = CommandResult::error(name, Error::Input);
                print_device(result, json_mode);
                std::process::exit(exit);
            }
            std::process::exit(report("unknown", json_mode, Err(Error::Input)));
        }
    };
    // Room sends carry durable lost-response recovery through the store, so
    // they run outside the plain device-command path.
    let rooms_send = match &args.command {
        Command::Rooms {
            command:
                RoomsCommand::Send {
                    room,
                    room_id,
                    thread,
                    text,
                },
        } => Some((room.clone(), room_id.clone(), thread.clone(), text.clone())),
        _ => None,
    };
    if let Some((room, room_id, thread, text)) = rooms_send {
        let name = "rooms send";
        let store = match default_config_dir().map(FileStore::new) {
            Ok(store) => store,
            Err(error) => {
                let (result, exit) = CommandResult::error(name, error);
                print_device(result, args.json);
                std::process::exit(exit);
            }
        };
        let client = store.load().and_then(HomeClient::new);
        let (result, exit) = match client {
            Ok(client) => {
                execute_room_send(
                    &store,
                    &client,
                    room_id,
                    room,
                    thread,
                    text,
                    std::time::Duration::from_secs(180),
                )
                .await
            }
            Err(error) => CommandResult::error(name, error),
        };
        print_device(result, args.json);
        std::process::exit(exit);
    }
    let old_command = match device_command(args.command) {
        Ok((command, duration)) => {
            let name = command.name();
            let client = default_config_dir()
                .and_then(|dir| FileStore::new(dir).load())
                .and_then(HomeClient::new);
            let (result, exit) = match client {
                Ok(client) => execute_device(&client, command, duration).await,
                Err(error) => CommandResult::error(name, error),
            };
            print_device(result, args.json);
            std::process::exit(exit);
        }
        Err(command) => command,
    };
    let command = match &old_command {
        Command::Pair { .. } => "pair",
        Command::Status => "status",
        Command::Bots { .. } => "bots list",
        _ => unreachable!("device commands were handled"),
    };
    let exit = report(command, args.json, execute(old_command).await);
    std::process::exit(exit);
}

#[cfg(test)]
mod tests {
    use super::{Args, duration};
    use clap::Parser;
    #[test]
    fn timeout_grammar_requires_digits_on_both_sides_of_decimal() {
        for input in [
            "1.s",
            "1.ms",
            "1.m",
            "1.",
            ".1s",
            "..1s",
            "1..2s",
            "+1s",
            "-1s",
            "1e3s",
            " 1s",
            "1s ",
            "1 s",
            "NaNs",
            "infs",
            "0",
            "0.0001s",
            "2147483648ms",
        ] {
            assert!(
                duration(input).is_err(),
                "accepted malformed timeout: {input}"
            );
            assert!(
                Args::try_parse_from(["ardur-rs", "wait", "--run", "run", "--timeout", input])
                    .is_err()
            );
        }
        for (input, millis) in [
            ("1", 1000),
            ("1s", 1000),
            ("1.5s", 1500),
            ("0.001s", 1),
            ("1ms", 1),
            ("0.5m", 30000),
            ("0001.0s", 1000),
            ("2147483647ms", 2147483647),
        ] {
            assert_eq!(duration(input).unwrap().as_millis(), millis, "{input}");
            assert!(
                Args::try_parse_from(["ardur-rs", "wait", "--run", "run", "--timeout", input])
                    .is_ok()
            );
        }
    }
}
