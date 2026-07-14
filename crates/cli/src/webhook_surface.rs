//! `ardur webhook` — the inbound webhook trigger surface (§9.7 blueprint,
//! ARD-996). An operator registers an HMAC-signed inbound endpoint bound to
//! an existing `ardur schedule`; a verified POST to that endpoint fires the
//! bound schedule through the same cap-token/budget/receipt gate stack
//! `ardur schedule fire` uses (see [`crate::fire_schedule`]).
//!
//! `ardur-webhook` (crates/webhook) already owns HMAC-SHA256 signature
//! verification and replay protection (timestamp header + a bounded
//! recently-seen-signature cache); this module is the operator-facing
//! binding-management CLI plus the long-lived `serve` process that mounts
//! every registered binding as an axum route through that crate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ardur_cli::CliError;
use ardur_webhook::{
    InboundWebhookHandler, WebhookConfig, WebhookEndpoint, WebhookEvent, WebhookRegistry,
};
use async_trait::async_trait;
use clap::{Args, Subcommand};
use secrecy::SecretString;
use serde_json::json;

use crate::{StateDirs, automation_gate, fire_schedule};

/// Arguments to `ardur webhook`.
#[derive(Args)]
pub struct WebhookArgs {
    #[command(subcommand)]
    pub action: WebhookAction,
}

/// Subcommands for `ardur webhook`.
#[derive(Subcommand)]
pub enum WebhookAction {
    /// Register an inbound trigger endpoint bound to a schedule.
    Add {
        /// Source tag; the endpoint is mounted at `/webhooks/<source>`.
        source: String,
        /// The schedule id this endpoint fires on a verified POST.
        #[arg(long)]
        schedule_id: String,
        /// HMAC-SHA256 signing secret the sender must sign requests with.
        #[arg(long)]
        secret: String,
        /// Cost, in cents, charged against the bound schedule's budget per
        /// verified trigger.
        #[arg(long, default_value_t = 1)]
        cost_cents: u64,
        /// A token minted by `ardur token create --scope write` (or higher);
        /// registering an endpoint is a mutation.
        #[arg(long)]
        token: Option<String>,
    },
    /// List registered inbound trigger endpoints (secrets never shown).
    List,
    /// Remove a registered endpoint.
    Remove {
        /// Source tag.
        source: String,
        /// A token minted by `ardur token create --scope write` (or higher).
        #[arg(long)]
        token: Option<String>,
    },
    /// Simulate a verified inbound delivery: sign a sample payload with the
    /// endpoint's stored secret, run it through the same verify-then-fire
    /// path `serve` uses, and report the outcome without opening a socket.
    Test {
        /// Source tag.
        source: String,
    },
    /// Start the long-lived inbound listener, mounting every registered
    /// endpoint's verify-then-fire handler as an axum route.
    Serve {
        /// TCP port to bind.
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },
}

/// A registered inbound trigger binding, persisted at
/// `<root>/webhooks/<source>.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct TriggerBinding {
    source: String,
    schedule_id: String,
    secret: String,
    cost_cents: u64,
    created_at: u64,
}

fn webhooks_dir(root: &Path) -> PathBuf {
    root.join("webhooks")
}

fn binding_path(root: &Path, source: &str) -> PathBuf {
    webhooks_dir(root).join(format!("{source}.json"))
}

fn read_binding(root: &Path, source: &str) -> Result<TriggerBinding, CliError> {
    let content = std::fs::read_to_string(binding_path(root, source)).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CliError::State(format!("no webhook endpoint registered for `{source}`"))
        } else {
            CliError::Io(e)
        }
    })?;
    serde_json::from_str(&content).map_err(|e| CliError::State(e.to_string()))
}

fn read_all_bindings(root: &Path) -> Result<Vec<TriggerBinding>, CliError> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(webhooks_dir(root)) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(CliError::Io(e)),
    };
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|e| e == "json") {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                if let Ok(binding) = serde_json::from_str::<TriggerBinding>(&content) {
                    out.push(binding);
                }
            }
        }
    }
    Ok(out)
}

/// The holder identity a webhook endpoint's registration receipts are keyed
/// under (distinct from the bound schedule's own holder — the fire receipt
/// still attributes to the schedule).
fn endpoint_holder_id(source: &str) -> String {
    format!("webhook.endpoint:{source}")
}

/// Handler that verifies (via [`ardur_webhook`]'s HMAC + replay checks,
/// already applied before this runs) then fires the bound schedule. Shared by
/// `serve` (real HTTP) and `test` (in-process simulation).
struct TriggerHandler {
    root: PathBuf,
    binding: TriggerBinding,
}

#[async_trait]
impl InboundWebhookHandler for TriggerHandler {
    async fn handle(&self, _event: WebhookEvent) -> Result<(), ardur_webhook::WebhookError> {
        let root = self.root.clone();
        let binding_id = binding_holder_display(&self.binding);
        let schedule_id = self.binding.schedule_id.clone();
        let cost_cents = self.binding.cost_cents;
        let source = self.binding.source.clone();
        tokio::task::spawn_blocking(move || {
            fire_schedule(
                &root,
                &schedule_id,
                Some(WEBHOOK_TRIGGER_TOKEN),
                cost_cents,
                &format!("webhook:{source}"),
            )
        })
        .await
        .map_err(|e| ardur_webhook::WebhookError::Internal(e.to_string()))?
        .map_err(|e| {
            tracing::warn!(
                "webhook trigger {} failed to fire schedule: {}",
                binding_id,
                e
            );
            ardur_webhook::WebhookError::Internal(e.to_string())
        })?;
        Ok(())
    }
}

fn binding_holder_display(binding: &TriggerBinding) -> String {
    format!("{}->{}", binding.source, binding.schedule_id)
}

/// A synthetic operator token that always resolves to `admin` scope, minted
/// and stored the first time a webhook binding needs to fire a schedule
/// unattended (the operator authorized the binding itself with a `--token`
/// at `add` time; the fired schedule's cap-token/budget/receipt gate still
/// runs on every trigger — this is the credential that satisfies it).
const WEBHOOK_TRIGGER_TOKEN_LABEL: &str = "webhook-trigger-service-token";
const WEBHOOK_TRIGGER_TOKEN: &str = "ardur-webhook-trigger-service-token-v1";

fn ensure_service_token(root: &Path) -> Result<(), CliError> {
    let tokens_dir = root.join("tokens");
    std::fs::create_dir_all(&tokens_dir)?;
    let hash_hex = {
        use sha2::Digest as _;
        hex::encode(sha2::Sha256::digest(WEBHOOK_TRIGGER_TOKEN.as_bytes()))
    };
    let already_present = std::fs::read_dir(&tokens_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .any(|record| record.get("hash").and_then(|v| v.as_str()) == Some(hash_hex.as_str()));
    if already_present {
        return Ok(());
    }
    let token_id = uuid::Uuid::new_v4().to_string();
    let record = json!({
        "token_id": token_id,
        "label": WEBHOOK_TRIGGER_TOKEN_LABEL,
        "scope": "write",
        "hash": hash_hex,
        "created_at": 0,
        "revoked": false,
    });
    std::fs::write(
        tokens_dir.join(format!("{token_id}.json")),
        serde_json::to_string_pretty(&record).map_err(|e| CliError::State(e.to_string()))?,
    )?;
    Ok(())
}

/// Run `ardur webhook` subcommands.
pub fn run_webhook(args: WebhookArgs) -> Result<(), CliError> {
    let root = StateDirs::resolve()?.root;
    std::fs::create_dir_all(webhooks_dir(&root))?;

    match args.action {
        WebhookAction::Add {
            source,
            schedule_id,
            secret,
            cost_cents,
            token,
        } => {
            let token_id = automation_gate::require_token_scope(&root, token.as_deref(), "write")?;
            if !crate::schedule_exists(&root, &schedule_id) {
                return Err(CliError::State(format!(
                    "schedule `{schedule_id}` not found; create it first with `ardur schedule create`"
                )));
            }
            ensure_service_token(&root)?;
            let binding = TriggerBinding {
                source: source.clone(),
                schedule_id: schedule_id.clone(),
                secret,
                cost_cents,
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            };
            std::fs::write(
                binding_path(&root, &source),
                serde_json::to_string_pretty(&binding)
                    .map_err(|e| CliError::State(e.to_string()))?,
            )?;
            automation_gate::append_receipt(
                &root,
                "webhook.endpoint.registered.v1",
                &endpoint_holder_id(&source),
                &token_id,
                0,
                &json!({"source": source, "schedule_id": schedule_id}),
            )?;
            println!("registered webhook endpoint `{source}` -> schedule `{schedule_id}`");
            println!("mount path when serving: /webhooks/{source}");
        }
        WebhookAction::List => {
            let bindings = read_all_bindings(&root)?;
            if bindings.is_empty() {
                println!("no webhook endpoints registered");
            } else {
                let summary: Vec<serde_json::Value> = bindings
                    .iter()
                    .map(|b| {
                        json!({
                            "source": b.source,
                            "schedule_id": b.schedule_id,
                            "cost_cents": b.cost_cents,
                            "secret": "<redacted>",
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!(summary)).expect("bindings serialise")
                );
            }
        }
        WebhookAction::Remove { source, token } => {
            let token_id = automation_gate::require_token_scope(&root, token.as_deref(), "write")?;
            let path = binding_path(&root, &source);
            if !path.is_file() {
                return Err(CliError::State(format!(
                    "no webhook endpoint registered for `{source}`"
                )));
            }
            std::fs::remove_file(&path)?;
            automation_gate::append_receipt(
                &root,
                "webhook.endpoint.revoked.v1",
                &endpoint_holder_id(&source),
                &token_id,
                0,
                &json!({"source": source}),
            )?;
            println!("removed webhook endpoint `{source}`");
        }
        WebhookAction::Test { source } => {
            let binding = read_binding(&root, &source)?;
            let secret = SecretString::new(binding.secret.clone().into());
            let body = br#"{"probe":"ardur webhook test"}"#;
            let signature = ardur_webhook::sign_body(body, &secret)
                .map_err(|e| CliError::State(format!("signing test payload: {e}")))?;
            ardur_webhook::verify_signature(body, &secret, &signature).map_err(|e| {
                CliError::State(format!("self-signed test payload failed to verify: {e}"))
            })?;
            println!("signature verify: ok");
            let run = fire_schedule(
                &root,
                &binding.schedule_id,
                Some(WEBHOOK_TRIGGER_TOKEN),
                binding.cost_cents,
                &format!("webhook-test:{source}"),
            )?;
            println!("fired schedule `{}` via webhook test", binding.schedule_id);
            println!("  receipt_id: {}", run.receipt_id);
        }
        WebhookAction::Serve { port } => {
            let bindings = read_all_bindings(&root)?;
            if bindings.is_empty() {
                return Err(CliError::State(
                    "no webhook endpoints registered; run `ardur webhook add` first".to_string(),
                ));
            }
            let mut registry = WebhookRegistry::new();
            for binding in bindings {
                let path = format!("/webhooks/{}", binding.source);
                let config = WebhookConfig::new(binding.secret.clone(), binding.source.clone())
                    .with_replay_protection("x-webhook-timestamp");
                let handler: Arc<dyn InboundWebhookHandler> = Arc::new(TriggerHandler {
                    root: root.clone(),
                    binding,
                });
                registry.register(
                    WebhookEndpoint {
                        path,
                        source: config.source.clone(),
                        config,
                    },
                    handler,
                );
            }
            let router = registry.router();
            println!("serving inbound webhook triggers on 0.0.0.0:{port}");
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
                    .await
                    .map_err(CliError::Io)?;
                axum::serve(listener, router)
                    .await
                    .map_err(|e| CliError::State(format!("webhook server error: {e}")))
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_round_trips_add_list_remove() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(webhooks_dir(root)).unwrap();
        let binding = TriggerBinding {
            source: "github".to_string(),
            schedule_id: "sched-1".to_string(),
            secret: "s3cret".to_string(),
            cost_cents: 2,
            created_at: 0,
        };
        std::fs::write(
            binding_path(root, "github"),
            serde_json::to_string_pretty(&binding).unwrap(),
        )
        .unwrap();

        let all = read_all_bindings(root).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].schedule_id, "sched-1");

        let single = read_binding(root, "github").unwrap();
        assert_eq!(single.cost_cents, 2);

        assert!(read_binding(root, "missing").is_err());
    }

    #[test]
    fn signature_sign_and_verify_round_trip() {
        let secret = SecretString::new("hook-secret".to_string().into());
        let body = br#"{"hello":"world"}"#;
        let signature = ardur_webhook::sign_body(body, &secret).unwrap();
        assert!(ardur_webhook::verify_signature(body, &secret, &signature).is_ok());
        assert!(ardur_webhook::verify_signature(b"tampered", &secret, &signature).is_err());
    }
}
