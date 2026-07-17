//! `ardur cron` — the operator-facing cron management surface (§9.4).
//!
//! A thin clap front-end over [`ardur_cron_ui`]. It mints an operator
//! cap-token scoped to the requested action (read-only by default; mutations
//! add the `cron.ui.mutate` scope, admin views add `cron.ui.admin`), then
//! drives the stateless [`CronController`] over the durable cron store at
//! `~/.ardur/cron/store.json`. Every action emits a signed receipt into
//! `~/.ardur/receipts/cron-ui.jsonl`.

use ardur_cap_token::{CapScope, CapTokenIssuer, HolderId, PublicKey};
use ardur_cli::{CliError, StateDirs};
use ardur_cron_ui::{
    CapGate, CreateRequest, CronController, CronFilter, CronMutation, DeliveryMode, Density,
    Es256ReceiptSink, FileCronStore, Principal, Redactor, SCOPE_ADMIN, SCOPE_MUTATE, SCOPE_VIEW,
    VisibilityTier, render_detail, render_list, validate_cron,
};
use clap::{Args, Subcommand};

/// The cron-UI cap-token audience.
const CRON_UI_AUDIENCE: &str = "cron-ui";
/// Operator token lifetime.
const TOKEN_TTL_SECS: u64 = 3_600;

/// Arguments to `ardur cron`.
#[derive(Args)]
pub struct CronArgs {
    #[command(subcommand)]
    action: CronAction,
}

/// Subcommands for `ardur cron`.
#[derive(Subcommand)]
enum CronAction {
    /// List scheduled crons (read-only).
    List {
        /// Filter expression: `status:errored`, `tag:<t>`, `channel:<c>`,
        /// `last-run:<secs>`, or free text over the name.
        #[arg(long)]
        filter: Option<String>,
        /// Show crons across all operators (requires admin authority).
        #[arg(long)]
        all: bool,
        /// Render density: compact | default | comfortable.
        #[arg(long, default_value = "default")]
        density: String,
        /// Emit JSON instead of a text table.
        #[arg(long)]
        json: bool,
    },
    /// Show a cron's detail + run history (read-only).
    Show {
        /// Cron id.
        id: String,
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Create a new cron (requires mutate authority).
    Create {
        /// Operator-facing name.
        #[arg(long)]
        name: String,
        /// 5-field cron expression (e.g. "0 9 * * 1").
        #[arg(long)]
        schedule: String,
        /// Prompt/mission to dispatch when the cron fires.
        #[arg(long)]
        prompt: String,
        /// Delivery target: `internal` (default), `webhook:<url>`, or `chat:<session-id>`.
        #[arg(long, default_value = "internal")]
        delivery: String,
        /// Optional per-cron model override.
        #[arg(long)]
        model: Option<String>,
        /// Optional mission tag.
        #[arg(long)]
        tag: Option<String>,
    },
    /// Pause a cron (requires mutate authority).
    Pause {
        /// Cron id.
        id: String,
    },
    /// Resume a paused cron (requires mutate authority).
    Resume {
        /// Cron id.
        id: String,
    },
    /// Delete a cron (requires mutate authority).
    Delete {
        /// Cron id.
        id: String,
    },
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build a controller over the durable store + receipt log, and mint an
/// operator [`Principal`] carrying exactly the requested scopes.
fn operator(
    dirs: &StateDirs,
    scopes: &[&str],
) -> Result<(CronController<FileCronStore, Es256ReceiptSink>, Principal), CliError> {
    let issuer = dirs.load_or_create_issuer()?;
    let cap_root: PublicKey = issuer.public_key();
    let now = now_unix();
    let token = issuer
        .issue(
            HolderId(dirs.local_subject()),
            CapScope {
                audience: CRON_UI_AUDIENCE.to_string(),
                expires_unix: now + TOKEN_TTL_SECS,
                budget_remaining: 1000,
                tool_allowlist: scopes.iter().map(|s| s.to_string()).collect(),
            },
        )
        .map_err(|e| CliError::State(format!("issue cap-token: {e}")))?
        .to_base64()
        .map_err(|e| CliError::State(format!("encode cap-token: {e}")))?;

    let gate = CapGate::new(cap_root, CRON_UI_AUDIENCE);
    let principal = gate
        .authorize(&token, now)
        .map_err(|e| CliError::State(format!("authorize operator: {e}")))?;

    let store = FileCronStore::new(dirs.root.join("cron").join("store.json"));
    let receipt_key = dirs.load_or_create_receipt_key()?;
    let sink = Es256ReceiptSink::new(receipt_key, dirs.receipts.join("cron-ui.jsonl"));
    Ok((CronController::new(store, sink), principal))
}

fn parse_delivery(spec: &str) -> Result<DeliveryMode, CliError> {
    if spec == "internal" {
        Ok(DeliveryMode::InternalOnly)
    } else if let Some(url) = spec.strip_prefix("webhook:") {
        Ok(DeliveryMode::Webhook {
            url: url.to_string(),
        })
    } else if let Some(session) = spec.strip_prefix("chat:") {
        Ok(DeliveryMode::ChatSession {
            session_id: session.to_string(),
        })
    } else {
        Err(CliError::State(format!(
            "unrecognized delivery target `{spec}` (expected internal | webhook:<url> | chat:<id>)"
        )))
    }
}

fn parse_density(s: &str) -> Density {
    match s {
        "compact" => Density::Compact,
        "comfortable" => Density::Comfortable,
        _ => Density::Default,
    }
}

/// Run `ardur cron` subcommands.
pub fn run_cron(args: CronArgs) -> Result<(), CliError> {
    let dirs = StateDirs::resolve()?;
    dirs.create()?;
    let now_ms = now_unix().saturating_mul(1000);

    match args.action {
        CronAction::List {
            filter,
            all,
            density,
            json,
        } => {
            // Admin view is the only one that needs an elevated scope; the
            // default is read-only Self visibility.
            let scopes: &[&str] = if all {
                &[SCOPE_VIEW, SCOPE_ADMIN]
            } else {
                &[SCOPE_VIEW]
            };
            let (controller, principal) = operator(&dirs, scopes)?;
            let visibility = if all {
                VisibilityTier::Tenant
            } else {
                VisibilityTier::SelfOnly
            };
            let redactor = Redactor::new();
            let filter = filter
                .map(|f| CronFilter::parse(&f, &redactor))
                .unwrap_or(CronFilter::All);
            let rows = controller.list(&principal, &filter, visibility, now_ms)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&rows)
                        .map_err(|e| CliError::State(e.to_string()))?
                );
            } else {
                println!("{}", render_list(&rows, parse_density(&density)));
            }
        }
        CronAction::Show { id, json } => {
            let (controller, principal) = operator(&dirs, &[SCOPE_VIEW])?;
            let detail = controller.detail(&principal, &id)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&detail)
                        .map_err(|e| CliError::State(e.to_string()))?
                );
            } else {
                println!("{}", render_detail(&detail));
            }
        }
        CronAction::Create {
            name,
            schedule,
            prompt,
            delivery,
            model,
            tag,
        } => {
            // Fail fast on a bad cron before minting a mutate token.
            validate_cron(&schedule).map_err(|e| CliError::State(e.to_string()))?;
            let delivery_mode = parse_delivery(&delivery)?;
            let (controller, principal) = operator(&dirs, &[SCOPE_VIEW, SCOPE_MUTATE])?;
            let report = controller.mutate(
                &principal,
                CronMutation::Create(CreateRequest {
                    name,
                    schedule_expr: schedule,
                    prompt,
                    delivery_mode,
                    model_override: model,
                    mission_tag: tag,
                }),
            )?;
            println!("created cron {}", report.cron_id);
            if let Some(rid) = report.receipt_id {
                println!("receipt {rid}");
            }
        }
        CronAction::Pause { id } => run_mutation(&dirs, CronMutation::Pause(id))?,
        CronAction::Resume { id } => run_mutation(&dirs, CronMutation::Resume(id))?,
        CronAction::Delete { id } => run_mutation(&dirs, CronMutation::Delete(id))?,
    }
    Ok(())
}

fn run_mutation(dirs: &StateDirs, mutation: CronMutation) -> Result<(), CliError> {
    let (controller, principal) = operator(dirs, &[SCOPE_VIEW, SCOPE_MUTATE])?;
    let label = mutation.label();
    let report = controller.mutate(&principal, mutation)?;
    println!("{label} cron {}", report.cron_id);
    if let Some(rid) = report.receipt_id {
        println!("receipt {rid}");
    }
    Ok(())
}
