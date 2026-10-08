use clap::{Parser, Subcommand};
use home_client::{
    Error, FileStore, HomeClient, SecretStore, default_config_dir, human_text, pair_device,
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
    Bots {
        #[command(subcommand)]
        command: BotsCommand,
    },
}
#[derive(Subcommand)]
enum BotsCommand {
    List,
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
    }
}
fn report(command: &str, json_mode: bool, result: Result<Value, Error>) -> i32 {
    match result {
        Ok(data) => {
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
                        json!({"schemaVersion":1,"ok":true,"command":"help","data":{"usage":"ardur-rs [--json] pair --file <path|-> [--name <name>] | status | bots list","version":env!("CARGO_PKG_VERSION")}})
                    )
                } else {
                    print!("{e}")
                }
                return;
            }
            // Do not echo invalid arguments (they may contain the pairing challenge).
            std::process::exit(report("unknown", json_mode, Err(Error::Input)));
        }
    };
    let command = match &args.command {
        Command::Pair { .. } => "pair",
        Command::Status => "status",
        Command::Bots { .. } => "bots list",
    };
    let exit = report(command, args.json, execute(args.command).await);
    std::process::exit(exit);
}
