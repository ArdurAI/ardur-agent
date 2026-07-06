//! Persona pack management CLI surface.

use std::path::{Path, PathBuf};

use ardur_cli::CliError;
use clap::{Args, Subcommand};

use crate::StateDirs;

/// Arguments to `ardur persona`.
#[derive(Args)]
pub struct PersonaArgs {
    #[command(subcommand)]
    pub action: PersonaAction,
}

/// Subcommands for `ardur persona`.
#[derive(Subcommand)]
pub enum PersonaAction {
    /// List installed personas.
    List,
    /// Show the currently active persona.
    Active,
    /// Set the active persona by name.
    Set { name: String },
    /// Create a persona from a JSON file or inline fields.
    Create {
        /// Persona name.
        name: String,
        /// Path to a persona JSON file.
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Show a persona definition.
    Show { name: String },
    /// Remove a persona.
    Remove { name: String },
    /// Install a domain pack from a directory.
    InstallPack {
        /// Path to the pack directory.
        path: PathBuf,
    },
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct PersonaRecord {
    name: String,
    display_name: String,
    system_prompt: String,
    domains: Vec<String>,
    tone: String,
    #[serde(default)]
    is_active: bool,
}

fn persona_dir(root: &Path) -> PathBuf {
    root.join("personas")
}

fn active_persona_path(root: &Path) -> PathBuf {
    persona_dir(root).join("active.json")
}

fn read_personas(root: &Path) -> Result<Vec<PersonaRecord>, CliError> {
    let dir = persona_dir(root);
    let mut records = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "json") {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(v) = serde_json::from_str::<PersonaRecord>(&content) {
                        records.push(v);
                    }
                }
            }
        }
    }
    Ok(records)
}

pub fn run_persona(args: PersonaArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;
    let dir = persona_dir(&root);
    std::fs::create_dir_all(&dir)?;

    match args.action {
        PersonaAction::List => {
            let records = read_personas(&root)?;
            if records.is_empty() {
                println!("no personas configured");
            } else {
                println!("NAME           DISPLAY NAME           ACTIVE");
                for r in &records {
                    let active = if r.is_active { "*" } else { "" };
                    println!("{: <14} {: <22} {}", r.name, r.display_name, active);
                }
            }
        }
        PersonaAction::Active => {
            let active_path = active_persona_path(&root);
            if active_path.is_file() {
                let content = std::fs::read_to_string(&active_path)?;
                let record: PersonaRecord =
                    serde_json::from_str(&content).map_err(|e| CliError::State(e.to_string()))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&record)
                        .map_err(|e| CliError::State(e.to_string()))?
                );
            } else {
                println!("no active persona");
            }
        }
        PersonaAction::Set { name } => {
            let records = read_personas(&root)?;
            let found = records.iter().any(|r| r.name == name);
            if !found {
                return Err(CliError::State(format!("persona `{name}` not found")));
            }
            for r in records {
                let mut r = r;
                r.is_active = r.name == name;
                let path = dir.join(format!("{}.json", r.name));
                std::fs::write(
                    &path,
                    serde_json::to_string_pretty(&r).map_err(|e| CliError::State(e.to_string()))?,
                )?;
            }
            std::fs::write(
                active_persona_path(&root),
                serde_json::to_string_pretty(&PersonaRecord {
                    name: name.clone(),
                    is_active: true,
                    ..Default::default()
                })
                .map_err(|e| CliError::State(e.to_string()))?,
            )?;
            println!("active persona set to {name}");
        }
        PersonaAction::Create { name, file } => {
            let mut record = PersonaRecord {
                name: name.clone(),
                display_name: name.clone(),
                system_prompt: "You are a helpful assistant.".to_string(),
                domains: Vec::new(),
                tone: "neutral".to_string(),
                is_active: false,
            };
            if let Some(path) = file {
                let content = std::fs::read_to_string(&path)?;
                record =
                    serde_json::from_str(&content).map_err(|e| CliError::State(e.to_string()))?;
                record.name = name.clone();
            }
            std::fs::write(
                dir.join(format!("{name}.json")),
                serde_json::to_string_pretty(&record)
                    .map_err(|e| CliError::State(e.to_string()))?,
            )?;
            println!("created persona {name}");
        }
        PersonaAction::Show { name } => {
            let records = read_personas(&root)?;
            let found = records.iter().find(|r| r.name == name);
            match found {
                Some(r) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(r)
                            .map_err(|e| CliError::State(e.to_string()))?
                    );
                }
                None => {
                    return Err(CliError::State(format!("persona `{name}` not found")));
                }
            }
        }
        PersonaAction::Remove { name } => {
            let path = dir.join(format!("{name}.json"));
            if !path.is_file() {
                return Err(CliError::State(format!("persona `{name}` not found")));
            }
            std::fs::remove_file(&path)?;
            let active_path = active_persona_path(&root);
            if active_path.is_file() {
                if let Ok(content) = std::fs::read_to_string(&active_path) {
                    if let Ok(record) = serde_json::from_str::<PersonaRecord>(&content) {
                        if record.name == name {
                            std::fs::remove_file(&active_path)?;
                        }
                    }
                }
            }
            println!("removed persona {name}");
        }
        PersonaAction::InstallPack { path } => {
            if !path.is_dir() {
                return Err(CliError::State(format!(
                    "pack path {} is not a directory",
                    path.display()
                )));
            }
            let mut installed = 0;
            for entry in std::fs::read_dir(&path)?.flatten() {
                let file = entry.path();
                if file.extension().is_some_and(|e| e == "json") {
                    let content = std::fs::read_to_string(&file)?;
                    let mut record: PersonaRecord = serde_json::from_str(&content)
                        .map_err(|e| CliError::State(e.to_string()))?;
                    let base = file
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&record.name)
                        .to_string();
                    record.name = base.clone();
                    std::fs::write(
                        dir.join(format!("{base}.json")),
                        serde_json::to_string_pretty(&record)
                            .map_err(|e| CliError::State(e.to_string()))?,
                    )?;
                    installed += 1;
                }
            }
            println!("installed {installed} personas from {}", path.display());
        }
    }
    Ok(())
}
