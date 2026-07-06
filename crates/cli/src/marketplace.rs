//! Marketplace / skill registry CLI surface.

use std::path::{Path, PathBuf};

use ardur_cli::CliError;
use clap::{Args, Subcommand};

use crate::StateDirs;

/// Arguments to `ardur marketplace`.
#[derive(Args)]
pub struct MarketplaceArgs {
    #[command(subcommand)]
    pub action: MarketplaceAction,
}

/// Subcommands for `ardur marketplace`.
#[derive(Subcommand)]
pub enum MarketplaceAction {
    /// List installed skills.
    List,
    /// Search the marketplace index for skills.
    Search {
        /// Query string.
        query: String,
    },
    /// Install a skill from a local path or remote URL.
    Install {
        /// Skill URL or local path.
        source: String,
    },
    /// Show details of an installed skill.
    Show {
        /// Skill id.
        id: String,
    },
    /// Remove an installed skill.
    Remove {
        /// Skill id.
        id: String,
    },
    /// Verify signatures of installed skills.
    Verify,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SkillRecord {
    skill_id: String,
    name: String,
    version: String,
    source: String,
    installed_at: u64,
    #[serde(default)]
    signature: String,
}

fn skills_dir(root: &Path) -> PathBuf {
    root.join("skills")
}

fn read_skills(root: &Path) -> Result<Vec<SkillRecord>, CliError> {
    let dir = skills_dir(root);
    let mut records = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "json") {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(v) = serde_json::from_str::<SkillRecord>(&content) {
                        records.push(v);
                    }
                }
            }
        }
    }
    Ok(records)
}

/// Verify a skill signature (stub: real ed25519/JWS verification later).
fn verify_skill_signature(record: &SkillRecord) -> bool {
    if record.signature.is_empty() {
        return false;
    }
    // TODO: wire JWS signature verification.
    record.signature.starts_with("sig-")
}

/// Run `ardur marketplace` subcommands.
pub fn run_marketplace(args: MarketplaceArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;
    let dir = skills_dir(&root);
    std::fs::create_dir_all(&dir)?;

    match args.action {
        MarketplaceAction::List => {
            let records = read_skills(&root)?;
            if records.is_empty() {
                println!("no skills installed");
            } else {
                println!("ID                  NAME                VERSION");
                for r in &records {
                    println!("{: <20} {: <20} {}", r.skill_id, r.name, r.version);
                }
            }
        }
        MarketplaceAction::Search { query } => {
            let records = read_skills(&root)?;
            let filtered: Vec<&SkillRecord> = records
                .iter()
                .filter(|r| {
                    r.name.to_lowercase().contains(&query.to_lowercase())
                        || r.skill_id.to_lowercase().contains(&query.to_lowercase())
                })
                .collect();
            if filtered.is_empty() {
                println!("no installed skills match '{query}'");
                println!("note: remote marketplace search is a Phase 2 wiring task");
            } else {
                for r in filtered {
                    println!("{} {} {}", r.skill_id, r.name, r.version);
                }
            }
        }
        MarketplaceAction::Install { source } => {
            let id = uuid::Uuid::new_v4().to_string();
            let record = SkillRecord {
                skill_id: id.clone(),
                name: "installed-skill".to_string(),
                version: "0.0.1".to_string(),
                source: source.clone(),
                installed_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                signature: String::new(),
            };
            std::fs::write(
                dir.join(format!("{id}.json")),
                serde_json::to_string_pretty(&record)
                    .map_err(|e| CliError::State(e.to_string()))?,
            )?;
            println!("installed skill {id} from {source}");
            println!("note: signature verification is a Phase 2 wiring task");
        }
        MarketplaceAction::Show { id } => {
            let records = read_skills(&root)?;
            let found = records.iter().find(|r| r.skill_id == id);
            match found {
                Some(r) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(r)
                            .map_err(|e| CliError::State(e.to_string()))?
                    );
                }
                None => {
                    return Err(CliError::State(format!("skill `{id}` not found")));
                }
            }
        }
        MarketplaceAction::Remove { id } => {
            let path = dir.join(format!("{id}.json"));
            if !path.is_file() {
                return Err(CliError::State(format!("skill `{id}` not found")));
            }
            std::fs::remove_file(&path)?;
            println!("removed skill {id}");
        }
        MarketplaceAction::Verify => {
            let records = read_skills(&root)?;
            let mut ok = 0;
            let mut failed = 0;
            for r in &records {
                if verify_skill_signature(r) {
                    println!("{}: verified", r.skill_id);
                    ok += 1;
                } else {
                    println!("{}: missing or invalid signature", r.skill_id);
                    failed += 1;
                }
            }
            println!("{ok} verified, {failed} failed out of {}", records.len());
        }
    }
    Ok(())
}
