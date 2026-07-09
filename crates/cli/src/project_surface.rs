//! Kanban + multi-agent project surface CLI.

use std::path::{Path, PathBuf};

use ardur_cli::CliError;
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::StateDirs;

/// Arguments to `ardur project`.
#[derive(Args)]
pub struct ProjectArgs {
    #[command(subcommand)]
    pub action: ProjectAction,
}

/// Subcommands for `ardur project`.
#[derive(Subcommand)]
pub enum ProjectAction {
    /// Print the project board and run ledger as JSON.
    Board,
    /// Add a Kanban card.
    AddCard {
        /// Card title.
        title: String,
        /// Initial card status.
        #[arg(long, default_value = "ready")]
        status: String,
        /// Owning human/agent lane.
        #[arg(long)]
        owner: Option<String>,
    },
    /// Move an existing card to a new status.
    Move {
        /// Card id.
        card_id: String,
        /// New status.
        status: String,
    },
    /// Record a multi-agent run with a receipt reference.
    RecordRun {
        /// Agent or lane that ran.
        #[arg(long)]
        agent: String,
        /// Human-readable run summary.
        #[arg(long)]
        summary: String,
        /// Receipt path, URL, or immutable run evidence handle.
        #[arg(long)]
        receipt: String,
        /// Optional related card id.
        #[arg(long)]
        card: Option<String>,
        /// Run status.
        #[arg(long, default_value = "completed")]
        status: String,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProjectSurface {
    #[serde(default)]
    cards: Vec<KanbanCard>,
    #[serde(default)]
    runs: Vec<AgentRun>,
}

#[derive(Debug, Serialize, Deserialize)]
struct KanbanCard {
    id: String,
    title: String,
    status: String,
    owner: Option<String>,
    created_at: u64,
    updated_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct AgentRun {
    id: String,
    agent: String,
    summary: String,
    receipt: String,
    card: Option<String>,
    status: String,
    recorded_at: u64,
}

fn surface_path(root: &Path) -> PathBuf {
    root.join("project-surface.json")
}

fn load_surface(path: &Path) -> Result<ProjectSurface, CliError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ProjectSurface::default()),
        Err(e) => return Err(CliError::Io(e)),
    };
    serde_json::from_str(&contents)
        .map_err(|e| CliError::State(format!("invalid project surface {}: {e}", path.display())))
}

fn save_surface(path: &Path, surface: &ProjectSurface) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(surface).map_err(|e| CliError::State(e.to_string()))?,
    )?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn validate_nonempty(field: &str, value: &str) -> Result<(), CliError> {
    if value.trim().is_empty() {
        return Err(CliError::State(format!("{field} must not be empty")));
    }
    Ok(())
}

fn find_card_mut<'a>(
    surface: &'a mut ProjectSurface,
    card_id: &str,
) -> Result<&'a mut KanbanCard, CliError> {
    surface
        .cards
        .iter_mut()
        .find(|card| card.id == card_id)
        .ok_or_else(|| CliError::State(format!("card `{card_id}` not found")))
}

/// Run `ardur project` subcommands.
pub fn run_project(args: ProjectArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;
    let path = surface_path(&root);
    let mut surface = load_surface(&path)?;

    match args.action {
        ProjectAction::Board => {
            println!(
                "{}",
                serde_json::to_string_pretty(&surface)
                    .map_err(|e| CliError::State(e.to_string()))?
            );
        }
        ProjectAction::AddCard {
            title,
            status,
            owner,
        } => {
            validate_nonempty("title", &title)?;
            validate_nonempty("status", &status)?;
            let now = now_secs();
            let card = KanbanCard {
                id: uuid::Uuid::now_v7().to_string(),
                title,
                status,
                owner,
                created_at: now,
                updated_at: now,
            };
            println!("added card {}", card.id);
            surface.cards.push(card);
            save_surface(&path, &surface)?;
        }
        ProjectAction::Move { card_id, status } => {
            validate_nonempty("status", &status)?;
            let card = find_card_mut(&mut surface, &card_id)?;
            card.status = status;
            card.updated_at = now_secs();
            println!("moved card {} to {}", card.id, card.status);
            save_surface(&path, &surface)?;
        }
        ProjectAction::RecordRun {
            agent,
            summary,
            receipt,
            card,
            status,
        } => {
            validate_nonempty("agent", &agent)?;
            validate_nonempty("summary", &summary)?;
            validate_nonempty("receipt", &receipt)?;
            validate_nonempty("status", &status)?;
            if let Some(card_id) = card.as_deref() {
                if !surface
                    .cards
                    .iter()
                    .any(|candidate| candidate.id == card_id)
                {
                    return Err(CliError::State(format!("card `{card_id}` not found")));
                }
            }
            let run = AgentRun {
                id: uuid::Uuid::now_v7().to_string(),
                agent,
                summary,
                receipt,
                card,
                status,
                recorded_at: now_secs(),
            };
            println!("recorded run {}", run.id);
            surface.runs.push(run);
            save_surface(&path, &surface)?;
        }
    }

    Ok(())
}
