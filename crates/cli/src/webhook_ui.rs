//! `ardur webhook` — the operator-facing webhook surface (§9.7).
//!
//! A thin clap front-end over [`ardur_webhook::WebhookOps`]. It mints an
//! operator cap-token scoped to the requested action (read-only listing by
//! default; register/emit/inbound add their scopes), then drives the gated
//! endpoint + trigger registries over durable stores under `~/.ardur/webhook/`.
//! Every action emits a signed receipt into `~/.ardur/receipts/webhook.jsonl`.
//! HMAC secrets are referenced by environment-variable name and never stored.

use ardur_cap_token::{CapScope, CapTokenIssuer, HolderId, PublicKey};
use ardur_cli::{CliError, StateDirs};
use ardur_webhook::{
    CapGate, DispatchRequest, DispatchResult, Dispatcher, EndpointRegistration, Es256ReceiptSink,
    JsonCollectionStore, Principal, SCOPE_ENDPOINT_READ, SCOPE_ENDPOINT_REGISTER,
    SCOPE_INBOUND_REGISTER, SCOPE_OUTBOUND_EMIT, TriggerRegistration, WebhookError, WebhookOps,
};
use clap::{Args, Subcommand};

/// The webhook-ops cap-token audience.
const WEBHOOK_AUDIENCE: &str = "webhook-ops";
/// Operator token lifetime.
const TOKEN_TTL_SECS: u64 = 3_600;

/// Arguments to `ardur webhook`.
#[derive(Args)]
pub struct WebhookArgs {
    #[command(subcommand)]
    action: WebhookAction,
}

/// Subcommands for `ardur webhook`.
#[derive(Subcommand)]
enum WebhookAction {
    /// Manage outbound webhook endpoints.
    Endpoint(EndpointArgs),
    /// Manage inbound webhook triggers.
    Trigger(TriggerArgs),
}

/// Arguments to `ardur webhook endpoint`.
#[derive(Args)]
struct EndpointArgs {
    #[command(subcommand)]
    action: EndpointAction,
}

/// Subcommands for `ardur webhook endpoint`.
#[derive(Subcommand)]
enum EndpointAction {
    /// Register a new outbound endpoint.
    Add {
        /// Operator-facing name.
        #[arg(long)]
        name: String,
        /// Destination URL.
        #[arg(long)]
        url: String,
        /// Name of the environment variable holding the HMAC secret (the secret
        /// value is never stored).
        #[arg(long)]
        secret_env: String,
        /// HTTP method (defaults to POST).
        #[arg(long)]
        method: Option<String>,
    },
    /// List your registered endpoints.
    List {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Revoke (soft-delete) an endpoint.
    Remove {
        /// Endpoint id.
        id: String,
    },
    /// Send a signed test payload to an endpoint.
    Test {
        /// Endpoint id.
        id: String,
        /// JSON payload to send (defaults to a probe object).
        #[arg(long)]
        payload: Option<String>,
    },
}

/// Arguments to `ardur webhook trigger`.
#[derive(Args)]
struct TriggerArgs {
    #[command(subcommand)]
    action: TriggerAction,
}

/// Subcommands for `ardur webhook trigger`.
#[derive(Subcommand)]
enum TriggerAction {
    /// Register a new inbound trigger.
    Add {
        /// Operator-facing name.
        #[arg(long)]
        name: String,
        /// Absolute route path (e.g. /hooks/github).
        #[arg(long)]
        path: String,
        /// Event source label.
        #[arg(long)]
        source: String,
        /// Environment variable holding the inbound HMAC secret.
        #[arg(long)]
        secret_env: String,
        /// Action the trigger dispatches when it fires.
        #[arg(long)]
        action: String,
    },
    /// List your registered inbound triggers.
    List {
        /// Emit JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Remove an inbound trigger.
    Remove {
        /// Trigger id.
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

/// Build the ops facade and mint an operator [`Principal`] with exactly the
/// requested scopes.
fn operator(
    dirs: &StateDirs,
    scopes: &[&str],
) -> Result<(WebhookOps<Es256ReceiptSink>, Principal), CliError> {
    let issuer = dirs.load_or_create_issuer()?;
    let cap_root: PublicKey = issuer.public_key();
    let now = now_unix();
    let token = issuer
        .issue(
            HolderId(dirs.local_subject()),
            CapScope {
                audience: WEBHOOK_AUDIENCE.to_string(),
                expires_unix: now + TOKEN_TTL_SECS,
                budget_remaining: 1000,
                tool_allowlist: scopes.iter().map(|s| s.to_string()).collect(),
            },
        )
        .map_err(|e| CliError::State(format!("issue cap-token: {e}")))?
        .to_base64()
        .map_err(|e| CliError::State(format!("encode cap-token: {e}")))?;

    let gate = CapGate::new(cap_root, WEBHOOK_AUDIENCE);
    let principal = gate
        .authorize(&token, now)
        .map_err(|e| CliError::State(format!("authorize operator: {e}")))?;

    let base = dirs.root.join("webhook");
    let endpoints = JsonCollectionStore::new(base.join("endpoints.json"));
    let triggers = JsonCollectionStore::new(base.join("triggers.json"));
    let receipt_key = dirs.load_or_create_receipt_key()?;
    let sink = Es256ReceiptSink::new(receipt_key, dirs.receipts.join("webhook.jsonl"));
    Ok((WebhookOps::new(endpoints, triggers, sink), principal))
}

/// A real HTTP dispatcher backing `webhook endpoint test`. Builds a
/// single-threaded runtime and blocks on the request (the CLI is short-lived).
struct ReqwestDispatcher {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

impl ReqwestDispatcher {
    fn new() -> Result<Self, CliError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(CliError::Io)?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| CliError::State(format!("http client: {e}")))?;
        Ok(Self { runtime, client })
    }
}

impl Dispatcher for ReqwestDispatcher {
    fn dispatch(&self, request: &DispatchRequest) -> Result<DispatchResult, WebhookError> {
        self.runtime.block_on(async {
            let method = reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|e| WebhookError::OutboundRequestFailed(e.to_string()))?;
            let mut builder = self
                .client
                .request(method, &request.url)
                .body(request.body.clone());
            for (k, v) in &request.headers {
                builder = builder.header(k, v);
            }
            let resp = builder
                .send()
                .await
                .map_err(|e| WebhookError::OutboundRequestFailed(e.to_string()))?;
            Ok(DispatchResult {
                status: resp.status().as_u16(),
            })
        })
    }
}

/// Run `ardur webhook` subcommands.
pub fn run_webhook(args: WebhookArgs) -> Result<(), CliError> {
    let dirs = StateDirs::resolve()?;
    dirs.create()?;

    match args.action {
        WebhookAction::Endpoint(e) => run_endpoint(&dirs, e.action),
        WebhookAction::Trigger(t) => run_trigger(&dirs, t.action),
    }
}

fn run_endpoint(dirs: &StateDirs, action: EndpointAction) -> Result<(), CliError> {
    match action {
        EndpointAction::Add {
            name,
            url,
            secret_env,
            method,
        } => {
            let (ops, principal) = operator(dirs, &[SCOPE_ENDPOINT_READ, SCOPE_ENDPOINT_REGISTER])?;
            let id = ops.register_endpoint(
                &principal,
                EndpointRegistration {
                    name,
                    url,
                    method,
                    secret_env,
                    signature_header: None,
                },
            )?;
            println!("registered endpoint {id}");
        }
        EndpointAction::List { json } => {
            let (ops, principal) = operator(dirs, &[SCOPE_ENDPOINT_READ])?;
            let endpoints = ops.list_endpoints(&principal)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&endpoints)
                        .map_err(|e| CliError::State(e.to_string()))?
                );
            } else if endpoints.is_empty() {
                println!("no endpoints");
            } else {
                for e in &endpoints {
                    let state = if e.revoked { "revoked" } else { "active" };
                    println!("{} {}  {}  [{}]  {}", e.id, state, e.name, e.method, e.url);
                }
            }
        }
        EndpointAction::Remove { id } => {
            let (ops, principal) = operator(dirs, &[SCOPE_ENDPOINT_READ, SCOPE_ENDPOINT_REGISTER])?;
            ops.revoke_endpoint(&principal, &id)?;
            println!("revoked endpoint {id}");
        }
        EndpointAction::Test { id, payload } => {
            let (ops, principal) = operator(dirs, &[SCOPE_ENDPOINT_READ, SCOPE_OUTBOUND_EMIT])?;
            let body = payload.unwrap_or_else(|| "{\"probe\":true}".to_string());
            let dispatcher = ReqwestDispatcher::new()?;
            let report = ops.emit(&principal, &id, body.as_bytes(), &dispatcher)?;
            match report.status {
                Some(status) if report.delivered => {
                    println!("delivered to {id} (HTTP {status})")
                }
                Some(status) => println!("failed: {id} returned HTTP {status}"),
                None => println!("failed: transport error reaching {id}"),
            }
            if let Some(rid) = report.receipt_id {
                println!("receipt {rid}");
            }
        }
    }
    Ok(())
}

fn run_trigger(dirs: &StateDirs, action: TriggerAction) -> Result<(), CliError> {
    match action {
        TriggerAction::Add {
            name,
            path,
            source,
            secret_env,
            action,
        } => {
            let (ops, principal) = operator(dirs, &[SCOPE_ENDPOINT_READ, SCOPE_INBOUND_REGISTER])?;
            let id = ops.register_trigger(
                &principal,
                TriggerRegistration {
                    name,
                    path,
                    source,
                    secret_env,
                    action,
                    replay_window_secs: None,
                },
            )?;
            println!("registered trigger {id}");
        }
        TriggerAction::List { json } => {
            let (ops, principal) = operator(dirs, &[SCOPE_ENDPOINT_READ])?;
            let triggers = ops.list_triggers(&principal)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&triggers)
                        .map_err(|e| CliError::State(e.to_string()))?
                );
            } else if triggers.is_empty() {
                println!("no triggers");
            } else {
                for t in &triggers {
                    println!(
                        "{} {}  {} <- {}  ({})",
                        t.id, t.name, t.action, t.path, t.source
                    );
                }
            }
        }
        TriggerAction::Remove { id } => {
            let (ops, principal) = operator(dirs, &[SCOPE_ENDPOINT_READ, SCOPE_INBOUND_REGISTER])?;
            ops.remove_trigger(&principal, &id)?;
            println!("removed trigger {id}");
        }
    }
    Ok(())
}
