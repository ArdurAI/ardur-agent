//! [`AppState`] — the wired, shareable application state, and the boot sequence
//! that assembles it.
//!
//! [`AppState::boot`] runs the deployment's one-time wiring: ensure the data
//! directory layout, load-or-mint the long-lived keys (the cap-token issuer and
//! the receipt signing key), compile the Cedar policy, construct the fused
//! [`FusedRuntime`] over the persistent journal + receipt log (memory is still
//! in-process — see the Phase-3 note below), and spawn the turn-processing
//! worker. The result is wrapped in an [`Arc`] and shared across every request
//! handler.
//!
//! # Why a worker thread
//!
//! [`FusedRuntime::submit`] awaits the lifecycle-hook registry, whose hooks are
//! `?Send` ([`LifecycleHook`](ardur_lifecycle_hooks::LifecycleHook) is
//! `#[async_trait(?Send)]`), so its future is **not** `Send` — it cannot be
//! driven on axum's multi-threaded handlers. A genuine inbound message is
//! therefore handed to a dedicated worker thread running a *current-thread*
//! Tokio runtime (whose `block_on` drives `!Send` futures), which mints the
//! turn's cap-token, runs the fused pipeline, and posts the reply. The HTTP
//! handler verifies the signature, enqueues the message, and returns `200`
//! immediately — the ack pattern Slack expects (it retries on a slow/non-2xx
//! response).
//!
//! # Cost-gate holder model (Phase-3 marker)
//!
//! The Phase-2 cost-gate keys a budget on the verified cap-token *subject* and
//! is provisioned only at build time ([`FusedRuntimeBuilder::provision_budget`]
//! has no request-time counterpart, and the runtime owns its budget store
//! privately after `build`). A single [`FusedRuntime`] is therefore built once
//! at boot with one provisioned holder — the fixed [`GATEWAY_SUBJECT`] — and
//! every minted session token is issued under that subject. The real Slack user
//! id and channel ride the per-turn tracing fields instead.
//!
//! `// TODO Phase 3:` per-Slack-user cap-token subjects with per-user / per-
//! session budget provisioning, which needs a request-time provisioning API on
//! the runtime (or an injectable shared budget store).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ardur_cap_token::{
    BiscuitCapTokenIssuer, CapScope, CapTokenIssuer, HolderId as CapHolderId, KeyPair,
};
use ardur_cedar_policy::{ActionRef, CedarPolicyBundle, PolicyBundle, PolicySource};
use ardur_cost_gate::{CostEnvelope, CostTuple as GateCostTuple, HolderId as GateHolderId};
use ardur_fused_runtime::{FusedRuntime, FusedRuntimeBuilder};
use ardur_memory::InMemoryMemoryRuntime;
use ardur_messaging_gateway::{IncomingMessage, MessageBody};
use ardur_provider_runtime::{ModelId, Provider};
use ardur_receipt::Es256SigningKey;
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};
use ardur_session_journals::{FileSessionJournal, SessionJournal};
use ardur_slack_adapter::SlackAdapter;
use biscuit_auth::{Algorithm, PrivateKey};
use secrecy::SecretString;
use tokio::sync::mpsc;

use crate::config::Config;

/// The audience every session cap-token is scoped to — and the single audience
/// the fused runtime verifies against (it is fixed at build time, so it cannot
/// vary per channel). See the module Phase-3 note.
pub const AUDIENCE: &str = "ardur";

/// The capability the chat turn exercises. Matches the runtime's verifier caveat
/// and the Cedar policy's bound.
pub const TOOL: &str = "chat.submit";

/// The fixed cost-gate holder / cap-token subject every session token is issued
/// under (see the module Phase-3 note on why this is not the per-Slack-user id).
pub const GATEWAY_SUBJECT: &str = "ardur:slack-gateway";

/// How long a freshly minted session cap-token is valid — five minutes, matching
/// the Slack replay window. A turn that outlives this re-mints on the next event.
pub const CAP_TTL_SECS: u64 = 5 * 60;

/// A safe per-turn cents reservation cap. The cost gate reserves this up front
/// and refunds down to the turn's actual cost at finalize, so the per-process
/// budget depletes by real spend; this only bounds a single turn's hold. Clamped
/// down to the configured budget so a tiny budget still admits its first turn.
const PER_TURN_CENTS_CAP: u64 = 1_000;

/// The built-in permissive-but-bounded Cedar policy: permit the one action the
/// server performs (`Action::Submit`) and nothing else. Used when
/// `ARDUR_CEDAR_POLICY_PATH` is unset or its file is absent.
const DEFAULT_POLICY: &str = "permit(principal, action == Action::\"Submit\", resource);";

/// The wired application state shared (behind an [`Arc`]) across request handlers.
///
/// Holds only what the HTTP layer needs: the Slack adapter (inbound signature
/// verification), the channel onto the turn-processing worker, the journal
/// handle (for graceful shutdown), and the data directory. The fused runtime and
/// the cap-token issuer live on the worker thread (see the module docs).
pub struct AppState {
    slack: Arc<SlackAdapter>,
    work_tx: mpsc::UnboundedSender<IncomingMessage>,
    journal: Arc<dyn SessionJournal>,
    data_dir: PathBuf,
}

impl AppState {
    /// Run the boot sequence and return the shared state.
    ///
    /// `provider` is injected so the binary can pass the live
    /// [`AnthropicProvider`](ardur_provider_runtime::AnthropicProvider) while
    /// tests pass [`AnthropicProvider::stub`](ardur_provider_runtime::AnthropicProvider::stub).
    ///
    /// # Errors
    /// Any I/O failure creating the data directories or reading/writing the
    /// persisted keys, a malformed key file, a Cedar policy that fails to
    /// compile, or a receipt log that cannot be read back to resume the chain.
    pub fn boot(config: &Config, provider: Arc<dyn Provider>) -> anyhow::Result<Arc<Self>> {
        // 1. Data-directory layout.
        let data_dir = config.data_dir.clone();
        let memory_dir = data_dir.join("memory");
        let journals_dir = data_dir.join("journals");
        let receipts_dir = data_dir.join("receipts");
        let keys_dir = data_dir.join("keys");
        for dir in [&memory_dir, &journals_dir, &receipts_dir, &keys_dir] {
            std::fs::create_dir_all(dir)
                .map_err(|e| anyhow::anyhow!("creating {}: {e}", dir.display()))?;
        }

        // 2. Long-lived keys: the cap-token issuer (Ed25519/Biscuit) and the
        //    receipt signing key (ES256), loaded if present else minted + saved.
        let issuer = load_or_mint_issuer(&keys_dir)?;
        let cap_root = issuer.public_key();
        let receipt_key = load_or_generate_receipt_key(&keys_dir)?;

        // 3. Policy: the operator's file if configured + present, else built-in.
        let policy = load_policy(config.cedar_policy_path.as_deref())?;

        // 4. Substrate sinks.
        //    `// TODO Phase 3:` memory has no on-disk backing yet — the §7.0
        //    crate ships only `InMemoryMemoryRuntime`. The `memory/` dir is
        //    created for the future pgvector/durable backend; today facts live
        //    in-process and do not survive a restart.
        let _ = &memory_dir;
        let memory = Arc::new(InMemoryMemoryRuntime::new());

        // One journal per process boot. The fused runtime appends every turn's
        // user + assistant messages here (fsynced per entry).
        let boot_session = SessionId::new();
        let journal: Arc<dyn SessionJournal> = Arc::new(
            FileSessionJournal::new(&journals_dir, boot_session)
                .map_err(|e| anyhow::anyhow!("opening session journal: {e}"))?,
        );

        let receipt_log = receipts_dir.join("chain.jsonl");

        // 5. The fused runtime. Single instance, single receipt chain-tail mutex
        //    — so receipts chain correctly across turns.
        let envelope = per_turn_envelope(config.cost_budget_cents);
        let budget = gateway_budget(config.cost_budget_cents);
        let runtime = FusedRuntimeBuilder::new(
            cap_root,
            policy,
            provider,
            receipt_key,
            ModelId::new(&config.model),
        )
        .audience(AUDIENCE)
        .tool(TOOL)
        .action(ActionRef("Action::Submit".to_string()))
        .principal_entity_type("User")
        .projected_envelope(envelope)
        .provision_budget(GateHolderId(GATEWAY_SUBJECT.to_string()), budget)
        .with_memory(memory)
        .with_journal(journal.clone())
        .receipt_log(&receipt_log)
        .build()
        .map_err(|e| anyhow::anyhow!("building fused runtime: {e}"))?;

        // 6. The Slack adapter (base URL overridable so tests point at a mock).
        let mut slack = SlackAdapter::new(
            SecretString::new(config.slack_bot_token.clone()),
            SecretString::new(config.slack_signing_secret.clone()),
            config.slack_app_id.clone(),
        );
        if let Some(base) = &config.slack_base_url {
            slack = slack.with_base_url(base.clone());
        }
        let slack = Arc::new(slack);

        // 7. The turn-processing worker (see module docs on why a thread).
        let processor = Processor {
            runtime,
            slack: slack.clone(),
            issuer,
            cap_budget_remaining: config.cost_budget_cents,
        };
        let work_tx = spawn_worker(processor);

        Ok(Arc::new(Self {
            slack,
            work_tx,
            journal,
            data_dir,
        }))
    }

    /// The Slack adapter, for inbound event verification in the HTTP handler.
    #[must_use]
    pub fn slack(&self) -> &SlackAdapter {
        &self.slack
    }

    /// Hand a verified inbound message to the processing worker. Returns `false`
    /// only if the worker has shut down (so the caller can log a drop).
    #[must_use]
    pub fn enqueue(&self, message: IncomingMessage) -> bool {
        self.work_tx.send(message).is_ok()
    }

    /// The session journal handle (used by graceful shutdown to fsync + close).
    #[must_use]
    pub fn journal(&self) -> &Arc<dyn SessionJournal> {
        &self.journal
    }

    /// The on-disk data directory this state persists to.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

/// The turn-processing core, owned by the worker thread. Drives the `!Send`
/// fused-runtime pipeline and posts the reply.
struct Processor {
    runtime: FusedRuntime,
    slack: Arc<SlackAdapter>,
    issuer: BiscuitCapTokenIssuer,
    cap_budget_remaining: u64,
}

impl Processor {
    /// Run one inbound message through the fused runtime and post the reply.
    async fn handle(&self, incoming: IncomingMessage) {
        let channel = channel_from_id(&incoming.channel_id.0);
        let user = incoming.sender.0.clone();
        let Some(text) = message_text(&incoming.body) else {
            tracing::debug!(%user, %channel, "ignoring message with no text body");
            return;
        };

        let token = match self.mint_session_token(now_unix()) {
            Ok(token) => token,
            Err(e) => {
                tracing::error!(%user, %channel, error = %e, "failed to mint session cap-token");
                return;
            }
        };

        let request = SubmitRequest {
            messages: vec![ChatMessage::user(text)],
            cap_token: CapTokenRef(token),
            session_id: SessionId::new(),
            requested_provider: None,
        };

        match self.runtime.submit(request).await {
            Ok(result) => {
                let reply = result.response.content;
                match self.slack.post_message(&channel, &reply, None).await {
                    Ok(ts) => tracing::info!(
                        %user,
                        %channel,
                        receipt_id = %result.receipt_id.0,
                        ts = %ts,
                        "turn completed and reply posted"
                    ),
                    Err(e) => {
                        tracing::error!(%user, %channel, error = %e, "failed to post reply");
                    }
                }
            }
            Err(e) => {
                tracing::error!(%user, %channel, error = %e, "turn failed");
                let apology = format!("Sorry, that turn failed: {e}");
                if let Err(post_err) = self.slack.post_message(&channel, &apology, None).await {
                    tracing::error!(
                        %user, %channel, error = %post_err, "failed to post failure notice"
                    );
                }
            }
        }
    }

    /// Mint a fresh, short-lived ([`CAP_TTL_SECS`]) cap-token (base64) for this
    /// turn: scoped to [`AUDIENCE`] / [`TOOL`], issued under [`GATEWAY_SUBJECT`].
    fn mint_session_token(&self, now_unix: u64) -> anyhow::Result<String> {
        let token = self.issuer.issue(
            CapHolderId(GATEWAY_SUBJECT.to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix: now_unix.saturating_add(CAP_TTL_SECS),
                budget_remaining: self.cap_budget_remaining,
                tool_allowlist: vec![TOOL.to_string()],
            },
        )?;
        Ok(token.to_base64()?)
    }
}

/// Spawn the worker thread: a current-thread Tokio runtime that drains the work
/// queue, processing each message to completion in arrival order. Returns the
/// sender the HTTP layer enqueues onto.
fn spawn_worker(processor: Processor) -> mpsc::UnboundedSender<IncomingMessage> {
    let (tx, mut rx) = mpsc::unbounded_channel::<IncomingMessage>();
    std::thread::Builder::new()
        .name("ardur-turn-worker".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "worker runtime failed to start");
                    return;
                }
            };
            // `block_on` drives the `!Send` per-turn futures on this one thread.
            rt.block_on(async move {
                while let Some(message) = rx.recv().await {
                    processor.handle(message).await;
                }
            });
        })
        .expect("spawning the ardur-turn-worker thread");
    tx
}

/// The per-turn cost envelope the gate reserves (then refunds to actual). All
/// dimensions are `u32`; the provisioned budget covers each.
fn per_turn_envelope(budget_cents: u64) -> CostEnvelope {
    let cents_max = PER_TURN_CENTS_CAP
        .min(budget_cents)
        .min(u64::from(u32::MAX)) as u32;
    CostEnvelope {
        tokens_in_max: 200_000,
        tokens_out_max: 16_384,
        cents_max,
        wall_ms_max: 600_000,
        attention_score_max: 1_000,
    }
}

/// The gateway holder's provisioned budget: the configured cents ceiling, with
/// the non-monetary dimensions set wide enough to cover any per-turn envelope
/// (so admission turns only on cents).
fn gateway_budget(budget_cents: u64) -> GateCostTuple {
    const WIDE: u64 = 1_000_000_000_000;
    GateCostTuple {
        tokens_in: WIDE,
        tokens_out: WIDE,
        cents: budget_cents,
        wall_ms: WIDE,
        attention_score: WIDE,
    }
}

/// Extract the raw Slack channel id from the gateway's namespaced channel id
/// (`slack://<app_id>/<channel>`), so the reply targets the same channel.
fn channel_from_id(channel_id: &str) -> String {
    channel_id
        .rsplit('/')
        .next()
        .unwrap_or(channel_id)
        .to_string()
}

/// The text to ask the model about, for the message bodies that carry one.
fn message_text(body: &MessageBody) -> Option<String> {
    match body {
        MessageBody::Text(t) | MessageBody::Markdown(t) => Some(t.clone()),
        MessageBody::Mention { body, .. } => Some(body.clone()),
        // An attachment-only message has nothing for the model to answer.
        MessageBody::Attachment { .. } => None,
    }
}

/// Current wall-clock time in Unix seconds (saturating to 0 before the epoch).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load the cap-token issuer key from `keys/issuer.key`, minting and persisting a
/// fresh Ed25519 root key on first boot.
///
/// The key is stored as the Biscuit private key's canonical hex (not PKCS#8 PEM
/// — `biscuit-auth` exposes no PEM writer for Ed25519), one line.
fn load_or_mint_issuer(keys_dir: &Path) -> anyhow::Result<BiscuitCapTokenIssuer> {
    let path = keys_dir.join("issuer.key");
    if path.exists() {
        let hex = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let private = PrivateKey::from_bytes_hex(hex.trim(), Algorithm::Ed25519)
            .map_err(|e| anyhow::anyhow!("parsing issuer key {}: {e}", path.display()))?;
        Ok(BiscuitCapTokenIssuer::new(KeyPair::from(&private)))
    } else {
        let keypair = KeyPair::new();
        let hex = keypair.private().to_bytes_hex();
        write_private(&path, &hex)?;
        Ok(BiscuitCapTokenIssuer::new(keypair))
    }
}

/// Load the ES256 receipt signing key from `keys/receipt.pem`, generating and
/// persisting one (PKCS#8 PEM) on first boot.
fn load_or_generate_receipt_key(keys_dir: &Path) -> anyhow::Result<Es256SigningKey> {
    let path = keys_dir.join("receipt.pem");
    if path.exists() {
        let pem = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        Es256SigningKey::from_pkcs8_pem(&pem)
            .map_err(|e| anyhow::anyhow!("parsing receipt key {}: {e}", path.display()))
    } else {
        let key = Es256SigningKey::generate();
        let pem = key
            .to_pkcs8_pem()
            .map_err(|e| anyhow::anyhow!("encoding receipt key: {e}"))?;
        write_private(&path, &pem)?;
        Ok(key)
    }
}

/// Compile the Cedar policy: the operator's file when configured and present,
/// otherwise the built-in [`DEFAULT_POLICY`].
fn load_policy(path: Option<&Path>) -> anyhow::Result<CedarPolicyBundle> {
    let source = match path {
        Some(p) if p.exists() => PolicySource::File(p.to_path_buf()),
        _ => PolicySource::Embedded(DEFAULT_POLICY.to_string()),
    };
    CedarPolicyBundle::load(source).map_err(|e| anyhow::anyhow!("compiling cedar policy: {e}"))
}

/// Write a secret to `path` with owner-only permissions where the platform
/// supports it (`0o600` on Unix).
fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow::anyhow!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
}
