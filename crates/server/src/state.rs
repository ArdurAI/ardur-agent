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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use ardur_cap_token::{
    BiscuitCapTokenIssuer, CapScope, CapTokenIssuer, HolderId as CapHolderId, KeyPair,
};
use ardur_cedar_policy::{ActionRef, CedarPolicyBundle, PolicyBundle, PolicySource};
use ardur_channel_discord::DiscordChannel;
use ardur_channel_matrix::MatrixChannel;
use ardur_channel_telegram::TelegramChannel;
use ardur_cost_gate::{CostEnvelope, CostTuple as GateCostTuple, HolderId as GateHolderId};
use ardur_fused_runtime::{FusedRuntime, FusedRuntimeBuilder, load_persisted_chain};
use ardur_memory::{InMemoryMemoryRuntime, MemoryRuntime};
use ardur_memory_qdrant::{
    Bm25Index, Embedder, FastEmbedEmbedder, HybridMemoryRetriever, QdrantMemoryConfig,
    QdrantMemoryRuntime,
};
use ardur_messaging_gateway::{IncomingMessage, MessageBody, MessagingGateway};
use ardur_provider_runtime::{ModelId, Provider};
use ardur_receipt::Es256SigningKey;
use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, RuntimeError, SessionId, SubmitRequest,
};
use ardur_session_journals::{FileSessionJournal, SessionJournal};
use ardur_slack_adapter::SlackAdapter;
use ardur_tool_registry::ToolRegistry;
use biscuit_auth::{Algorithm, PrivateKey};
use secrecy::SecretString;
use tokio::sync::{mpsc, oneshot};

use crate::config::{Config, MemoryBackend};

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

/// The built-in development Cedar policy: permit chat submission plus the
/// tool-invocation action that is still constrained by cap-token tool caveats.
/// It is available only when `ARDUR_DEV_PERMISSIVE_POLICY=true` is explicitly set.
const DEFAULT_POLICY: &str = r#"
permit(principal, action == Action::"Submit", resource);
permit(principal, action == Action::"ToolInvoke", resource);
"#;

/// Production fallback when no explicit Cedar policy is configured: deny every
/// action. This keeps missing policy configuration fail-closed.
const DENY_ALL_POLICY: &str = "forbid(principal, action, resource);";

/// A unit of work handed to the turn worker. Both variants run the identical
/// fused-runtime pipeline; they differ only in where the reply goes — a
/// [`Channel`](WorkItem::Channel) message's reply is posted back to its origin
/// channel (Slack/Matrix/…), while an [`Http`](WorkItem::Http) `/chat` turn's
/// result is returned to the HTTP caller over a oneshot.
enum WorkItem {
    /// A fire-and-forget message from a chat channel; reply posted to the channel.
    Channel(IncomingMessage),
    /// A synchronous `POST /chat` turn; result returned over the oneshot.
    Http(HttpTurn),
}

/// A synchronous chat turn submitted over `POST /chat`: the prompt, the session
/// it belongs to, and the oneshot the worker sends the outcome (or the turn's
/// [`RuntimeError`]) back on.
struct HttpTurn {
    message: String,
    session_id: SessionId,
    reply: oneshot::Sender<Result<ChatTurnOutcome, RuntimeError>>,
}

/// The result of a successful synchronous `/chat` turn, surfaced to the HTTP
/// handler so it can render the response JSON.
#[derive(Clone, Debug)]
pub struct ChatTurnOutcome {
    /// The session the turn ran under — echoed so the caller can thread a
    /// follow-up onto the same session.
    pub session_id: SessionId,
    /// The assistant's reply text.
    pub reply: String,
    /// Prompt/input tokens billed for the turn.
    pub tokens_in: u64,
    /// Completion/output tokens billed for the turn.
    pub tokens_out: u64,
    /// Monetary cost of the turn, in whole US cents.
    pub cents: u64,
    /// The tools the model invoked over the turn's provider iterations, in
    /// receipt order. Empty when the turn took no tool calls.
    pub tools_called: Vec<String>,
    /// The id of the (final) receipt minted for the turn.
    pub receipt_id: String,
}

/// Why a `/chat` turn could not be completed.
#[derive(Debug, thiserror::Error)]
pub enum ChatSubmitError {
    /// The fused runtime rejected or failed the turn (cap-token, policy, cost
    /// gate, injection defense, or provider). The HTTP layer maps this to `502`.
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    /// The turn worker has shut down, so no turn can be processed.
    #[error("turn worker is unavailable")]
    WorkerGone,
}

/// The wired application state shared (behind an [`Arc`]) across request handlers.
///
/// Holds only what the HTTP layer needs: the Slack adapter (inbound signature
/// verification), the channel onto the turn-processing worker, the journal
/// handle (for graceful shutdown), and the data directory. The fused runtime and
/// the cap-token issuer live on the worker thread (see the module docs).
pub struct AppState {
    slack: Arc<SlackAdapter>,
    work_tx: Arc<Mutex<Option<mpsc::UnboundedSender<WorkItem>>>>,
    /// The OS-thread handle for the turn worker — used by [`shutdown`](Self::shutdown)
    /// to join the worker after closing the work channel. `None` in test harnesses
    /// that construct an `AppState` without spawning a real worker.
    worker_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    journal: Arc<dyn SessionJournal>,
    data_dir: PathBuf,
    chat_bearer_tokens: Vec<String>,
    admin_bearer_tokens: Vec<String>,
    tool_allowlist: Vec<String>,
    cost_budget_cents: u64,
    mcp: Option<McpSurface>,
    /// The Matrix channel, once [`attach_matrix`](AppState::attach_matrix) wires
    /// it (only when `ARDUR_CHANNEL_MATRIX=true`). Shared with the worker's
    /// [`Processor`] through the same `OnceLock`, so setting it here makes the
    /// reply path visible to the worker too. Empty when Matrix is disabled.
    matrix: Arc<OnceLock<Arc<MatrixChannel>>>,
    /// The Discord channel, once [`attach_discord`](AppState::attach_discord)
    /// wires it (only when `ARDUR_CHANNEL_DISCORD=true`). Same `OnceLock`-shared
    /// reply path as Matrix.
    discord: Arc<OnceLock<Arc<DiscordChannel>>>,
    /// The Telegram channel, once [`attach_telegram`](AppState::attach_telegram)
    /// wires it (only when `ARDUR_CHANNEL_TELEGRAM=true`). Same `OnceLock`-shared
    /// reply path as Matrix.
    telegram: Arc<OnceLock<Arc<TelegramChannel>>>,
}

/// The data [`build_router`](crate::build_router) needs to mount the §6.0 MCP
/// surface: the registry of locally-exposed tools, the bearer allowlist, and the
/// URL path prefix. `None` when `ARDUR_MCP_ENABLED` is unset.
pub struct McpSurface {
    /// The tools exposed over MCP (echo + health-check).
    pub registry: Arc<ardur_tool_registry::ToolRegistry>,
    /// The bearer tokens admitted to the MCP routes.
    pub bearer_tokens: Vec<String>,
    /// The URL path prefix the MCP routes mount under.
    pub path_prefix: String,
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
    pub fn boot(
        config: &Config,
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
    ) -> anyhow::Result<Arc<Self>> {
        // `tools` is the §6.0 registry the fused runtime invokes (local tools plus
        // any remote MCP toolsets the caller connected from
        // `ARDUR_MCP_REMOTE_SERVERS`). It is also what the MCP surface re-exposes
        // when `ARDUR_MCP_ENABLED` is set.

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
        let policy = load_policy(
            config.cedar_policy_path.as_deref(),
            config.dev_permissive_policy,
        )?;

        // 4. Substrate sinks. Memory is selected by `ARDUR_MEMORY`: the
        //    in-process §7.0 Phase 1 store (default — fast, lost on restart), the
        //    durable Qdrant-backed §7.0 Phase 2 store, or the §7.0c `hybrid`
        //    retriever (dense Qdrant + sparse BM25 + an embedder, fused on
        //    recall). All three implement the same `MemoryRuntime`, so the runtime
        //    builder is agnostic. The `memory/` dir is the file-backed BM25 index
        //    home under `hybrid`.
        let memory: Arc<dyn MemoryRuntime + Send + Sync> = match config.memory_backend {
            MemoryBackend::InMemory => {
                let _ = &memory_dir;
                Arc::new(InMemoryMemoryRuntime::new())
            }
            MemoryBackend::Qdrant => {
                let _ = &memory_dir;
                Arc::new(
                    QdrantMemoryRuntime::connect_and_init(qdrant_config(config))
                        .map_err(|e| anyhow::anyhow!("connecting qdrant memory: {e}"))?,
                )
            }
            MemoryBackend::Hybrid => {
                // Connect the durable half *un-initialised* — the collection's
                // vector dim is realigned to the embedder when the retriever
                // attaches it, so init must follow construction, never precede it.
                let qdrant = QdrantMemoryRuntime::connect(qdrant_config(config))
                    .map_err(|e| anyhow::anyhow!("connecting hybrid qdrant memory: {e}"))?;
                // The sparse half is a file-backed BM25 index under `memory/bm25`
                // so the lexical index survives restarts like the durable store.
                let bm25 = Bm25Index::new(Some(memory_dir.join("bm25")))
                    .map_err(|e| anyhow::anyhow!("opening hybrid bm25 index: {e}"))?;
                // The shared embedding model (BGE-small by default; `EMBED_MODEL`
                // overrides). Downloaded + disk-cached on first boot.
                let embedder: Arc<dyn Embedder> = Arc::new(
                    FastEmbedEmbedder::from_env()
                        .map_err(|e| anyhow::anyhow!("loading hybrid embedder: {e}"))?,
                );
                let hybrid = HybridMemoryRetriever::new(qdrant, bm25, embedder);
                // Init the durable collection now that the embedder is attached.
                hybrid
                    .qdrant()
                    .init()
                    .map_err(|e| anyhow::anyhow!("initialising hybrid qdrant collection: {e}"))?;
                Arc::new(hybrid)
            }
        };

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
        .with_tools(tools.clone())
        .receipt_log(&receipt_log)
        .build()
        .map_err(|e| anyhow::anyhow!("building fused runtime: {e}"))?;

        // 6. The Slack adapter (base URL overridable so tests point at a mock).
        let mut slack = SlackAdapter::new(
            SecretString::from(config.slack_bot_token.clone()),
            SecretString::from(config.slack_signing_secret.clone()),
            config.slack_app_id.clone(),
        )
        .with_allowed_senders(config.slack_allowed_senders.clone());
        if let Some(base) = &config.slack_base_url {
            slack = slack.with_base_url(base.clone());
        }
        let slack = Arc::new(slack);

        // 7. The turn-processing worker (see module docs on why a thread). The
        //    Matrix slot is shared with the worker so a Matrix-origin turn can be
        //    replied to through the Matrix channel; it stays empty unless
        //    `attach_matrix` later fills it.
        let matrix: Arc<OnceLock<Arc<MatrixChannel>>> = Arc::new(OnceLock::new());
        let discord: Arc<OnceLock<Arc<DiscordChannel>>> = Arc::new(OnceLock::new());
        let telegram: Arc<OnceLock<Arc<TelegramChannel>>> = Arc::new(OnceLock::new());
        let tool_allowlist = tool_allowlist_for_runtime(&tools);
        let processor = Processor {
            runtime,
            slack: slack.clone(),
            matrix: matrix.clone(),
            discord: discord.clone(),
            telegram: telegram.clone(),
            issuer,
            cap_budget_remaining: config.cost_budget_cents,
            tool_allowlist: tool_allowlist.clone(),
            receipt_log,
        };
        let (work_tx, worker_handle) = spawn_worker(processor);

        // 8. The MCP surface (opt-in). The same `tools` registry the runtime
        //    invokes is re-exposed over MCP; `build_router` mounts the
        //    bearer-gated routes when present.
        let mcp = if config.mcp_enabled {
            tracing::info!(
                tools = tools.list().len(),
                prefix = %config.mcp_path_prefix,
                "MCP surface enabled"
            );
            Some(McpSurface {
                registry: tools.clone(),
                bearer_tokens: config.mcp_bearer_tokens.clone(),
                path_prefix: config.mcp_path_prefix.clone(),
            })
        } else {
            None
        };

        Ok(Arc::new(Self {
            slack,
            work_tx: Arc::new(Mutex::new(Some(work_tx))),
            worker_handle: Mutex::new(Some(worker_handle)),
            journal,
            data_dir,
            chat_bearer_tokens: config.chat_bearer_tokens.clone(),
            admin_bearer_tokens: config.admin_bearer_tokens.clone(),
            tool_allowlist,
            cost_budget_cents: config.cost_budget_cents,
            mcp,
            matrix,
            discord,
            telegram,
        }))
    }

    /// The bearer tokens admitted to `POST /chat`.
    #[must_use]
    pub fn chat_bearer_tokens(&self) -> &[String] {
        &self.chat_bearer_tokens
    }

    /// The bearer tokens admitted to the admin runtime-inspection API.
    #[must_use]
    pub fn admin_bearer_tokens(&self) -> &[String] {
        &self.admin_bearer_tokens
    }

    /// The configured per-process cost-gate budget, in cents.
    #[must_use]
    pub fn cost_budget_cents(&self) -> u64 {
        self.cost_budget_cents
    }

    /// The tool ids minted into session cap-tokens for runtime turns.
    #[must_use]
    pub fn tool_allowlist(&self) -> &[String] {
        &self.tool_allowlist
    }

    /// Whether the turn worker is still accepting work.
    #[must_use]
    pub fn worker_alive(&self) -> bool {
        self.work_sender().is_some_and(|tx| !tx.is_closed())
    }

    /// Number of receipts currently persisted in the server's chain log.
    #[must_use]
    pub fn receipt_count(&self) -> usize {
        load_persisted_chain(self.data_dir.join("receipts").join("chain.jsonl"))
            .map(|chain| chain.len())
            .unwrap_or(0)
    }

    /// The MCP surface to mount, if `ARDUR_MCP_ENABLED` was set at boot.
    #[must_use]
    pub fn mcp(&self) -> Option<&McpSurface> {
        self.mcp.as_ref()
    }

    /// Wire a connected Matrix channel into the running server: record it for the
    /// worker's reply path, start its sync loop, and spawn a task that forwards
    /// each inbound Matrix message onto the same work queue Slack uses.
    ///
    /// Called by the binary after [`boot`](Self::boot) when
    /// `ARDUR_CHANNEL_MATRIX=true`. Must run inside a Tokio runtime (the bin's
    /// `#[tokio::main]`), since it spawns the sync and forwarding tasks. Calling
    /// it more than once is a no-op for the reply slot (the first channel wins).
    pub fn attach_matrix(&self, matrix: Arc<MatrixChannel>) {
        // Share the channel with the worker's `Processor` (same `OnceLock`).
        if self.matrix.set(matrix.clone()).is_err() {
            tracing::warn!("matrix channel already attached; ignoring the second attach");
            return;
        }

        matrix.start_sync();

        // Drain inbound Matrix messages onto the worker queue — the same path a
        // verified Slack message takes, so the fused turn runs identically and
        // the worker routes the reply back through Matrix by channel-id scheme.
        spawn_inbound_forwarder("matrix", Arc::clone(&self.work_tx), matrix);
    }

    /// Wire a connected Discord channel into the running server: record it for
    /// the worker's reply path, start its gateway loop, and forward each inbound
    /// Discord message onto the same work queue Slack uses.
    ///
    /// Called by the binary after [`boot`](Self::boot) when
    /// `ARDUR_CHANNEL_DISCORD=true`. Must run inside a Tokio runtime. Calling it
    /// more than once is a no-op for the reply slot (the first channel wins).
    pub async fn attach_discord(&self, discord: Arc<DiscordChannel>) {
        if self.discord.set(discord.clone()).is_err() {
            tracing::warn!("discord channel already attached; ignoring the second attach");
            return;
        }
        discord.start().await;
        spawn_inbound_forwarder("discord", Arc::clone(&self.work_tx), discord);
    }

    /// Wire a connected Telegram channel into the running server: record it for
    /// the worker's reply path, start its long-poll dispatcher, and forward each
    /// inbound Telegram message onto the same work queue Slack uses.
    ///
    /// Called by the binary after [`boot`](Self::boot) when
    /// `ARDUR_CHANNEL_TELEGRAM=true`. Must run inside a Tokio runtime. Calling it
    /// more than once is a no-op for the reply slot (the first channel wins).
    pub fn attach_telegram(&self, telegram: Arc<TelegramChannel>) {
        if self.telegram.set(telegram.clone()).is_err() {
            tracing::warn!("telegram channel already attached; ignoring the second attach");
            return;
        }
        telegram.start();
        spawn_inbound_forwarder("telegram", Arc::clone(&self.work_tx), telegram);
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
        self.work_sender()
            .is_some_and(|tx| tx.send(WorkItem::Channel(message)).is_ok())
    }

    /// Run a synchronous chat turn (the `POST /chat` path): hand the prompt to the
    /// turn worker and await its outcome over a oneshot. Unlike a channel message
    /// ([`enqueue`](Self::enqueue)), the reply is returned to the caller here
    /// rather than posted back to an origin channel — so an embedding HTTP client
    /// gets the assistant's response, its token/cost accounting, the tools it
    /// called, and the receipt id in one request/response.
    ///
    /// # Errors
    /// [`ChatSubmitError::Runtime`] if the fused runtime rejects or fails the turn
    /// (cap-token / policy / cost-gate / injection / provider), or
    /// [`ChatSubmitError::WorkerGone`] if the worker thread has shut down.
    pub async fn submit_chat(
        &self,
        message: String,
        session_id: SessionId,
    ) -> Result<ChatTurnOutcome, ChatSubmitError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let turn = HttpTurn {
            message,
            session_id,
            reply: reply_tx,
        };
        let Some(work_tx) = self.work_sender() else {
            return Err(ChatSubmitError::WorkerGone);
        };
        if work_tx.send(WorkItem::Http(turn)).is_err() {
            return Err(ChatSubmitError::WorkerGone);
        }
        match reply_rx.await {
            Ok(result) => result.map_err(ChatSubmitError::Runtime),
            // The worker dropped the sender without replying (it shut down).
            Err(_canceled) => Err(ChatSubmitError::WorkerGone),
        }
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

    /// Signal the background worker to drain, then join its OS thread.
    ///
    /// Taking the only long-lived sender closes the queue after any in-flight
    /// send completes. Forwarder tasks borrow the sender through the same mutex
    /// instead of holding permanent clones, so the worker can observe closure and
    /// exit before the process closes the journal.
    pub fn shutdown(&self) {
        tracing::info!("graceful shutdown requested");
        let sender = self.work_tx.lock().expect("work_tx mutex poisoned").take();
        drop(sender);

        let worker_handle = self
            .worker_handle
            .lock()
            .expect("worker_handle mutex poisoned")
            .take();
        if let Some(handle) = worker_handle {
            match handle.join() {
                Ok(()) => tracing::info!("turn worker shut down cleanly"),
                Err(_panic) => tracing::error!("turn worker panicked during shutdown"),
            }
        }
    }

    fn work_sender(&self) -> Option<mpsc::UnboundedSender<WorkItem>> {
        self.work_tx
            .lock()
            .expect("work_tx mutex poisoned")
            .as_ref()
            .cloned()
    }
}

fn tool_allowlist_for_runtime(tools: &ToolRegistry) -> Vec<String> {
    let mut allowlist = vec![
        TOOL.to_string(),
        // Memory capabilities — required so the fused runtime can re-verify
        // the cap token for memory.write before recording a turn, and so
        // memory.read/list/show authorizations pass.
        ardur_memory::MEMORY_READ_CAPABILITY.to_string(),
        ardur_memory::MEMORY_WRITE_CAPABILITY.to_string(),
    ];
    for tool in tools.list() {
        let id = tool.id().0;
        if !allowlist.contains(&id) {
            allowlist.push(id);
        }
        // Also add each tool's declared capabilities as `cap.*` strings so
        // ARD-420's `authorize_tool_capabilities` check can pass in production.
        for cap in tool.required_capabilities() {
            let label = cap.as_str();
            if !allowlist.contains(&label) {
                allowlist.push(label);
            }
        }
    }
    allowlist
}

/// The turn-processing core, owned by the worker thread. Drives the `!Send`
/// fused-runtime pipeline and posts the reply.
struct Processor {
    runtime: FusedRuntime,
    slack: Arc<SlackAdapter>,
    /// The Matrix channel, shared with [`AppState`]; `None` until attached. Used
    /// to post the reply when a turn originated on Matrix (`matrix://…`).
    matrix: Arc<OnceLock<Arc<MatrixChannel>>>,
    /// The Discord channel; used to reply when a turn originated on Discord
    /// (`discord://…`). `None` until attached.
    discord: Arc<OnceLock<Arc<DiscordChannel>>>,
    /// The Telegram channel; used to reply when a turn originated on Telegram
    /// (`telegram://…`). `None` until attached.
    telegram: Arc<OnceLock<Arc<TelegramChannel>>>,
    issuer: BiscuitCapTokenIssuer,
    cap_budget_remaining: u64,
    tool_allowlist: Vec<String>,
    /// The append-only receipt-chain log the fused runtime writes to. Read
    /// before/after an HTTP turn to recover the tools it called (the worker is
    /// single-threaded, so the receipts appended across one `submit` belong to
    /// exactly that turn).
    receipt_log: PathBuf,
}

/// Which channel backend a turn originated on — decided by the namespaced
/// channel-id scheme, and used to route the reply back to the same backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Slack,
    Matrix,
    Discord,
    Telegram,
}

impl Origin {
    /// Classify a namespaced channel id by its `scheme://` prefix.
    fn of(channel_id: &str) -> Self {
        if channel_id.starts_with("matrix://") {
            Origin::Matrix
        } else if channel_id.starts_with("discord://") {
            Origin::Discord
        } else if channel_id.starts_with("telegram://") {
            Origin::Telegram
        } else {
            Origin::Slack
        }
    }
}

impl Processor {
    /// Run one inbound message through the fused runtime and post the reply.
    async fn handle(&self, incoming: IncomingMessage) {
        // The channel-id scheme tells us which backend to reply through; the
        // last path segment is the provider's own channel/room/chat id.
        let origin = Origin::of(&incoming.channel_id.0);
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
                match self.post_reply(origin, &channel, &reply).await {
                    Ok(id) => tracing::info!(
                        %user,
                        %channel,
                        receipt_id = %result.receipt_id.0,
                        provider_message_id = %id,
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
                if let Err(post_err) = self.post_reply(origin, &channel, &apology).await {
                    tracing::error!(
                        %user, %channel, error = %post_err, "failed to post failure notice"
                    );
                }
            }
        }
    }

    /// Run one synchronous `/chat` turn through the fused runtime and return the
    /// outcome over the turn's oneshot. The reply is *not* posted to any channel —
    /// the HTTP caller receives it directly (see [`AppState::submit_chat`]).
    async fn handle_http(&self, turn: HttpTurn) {
        let HttpTurn {
            message,
            session_id,
            reply,
        } = turn;

        let token = match self.mint_session_token(now_unix()) {
            Ok(token) => token,
            Err(e) => {
                let _ = reply.send(Err(RuntimeError::Internal(anyhow::anyhow!(
                    "minting session cap-token: {e}"
                ))));
                return;
            }
        };

        // Bracket the turn's receipts: the worker is single-threaded and runs one
        // turn at a time, so every receipt appended between this snapshot and the
        // post-submit read belongs to this turn. The final receipt carries no tool
        // calls (it is the no-tool answer that terminates the loop); the tools ride
        // on the earlier tool-use iterations, which this window captures.
        let receipts_before = self.receipt_count();

        let request = SubmitRequest {
            messages: vec![ChatMessage::user(message)],
            cap_token: CapTokenRef(token),
            session_id,
            requested_provider: None,
        };

        let outcome = match self.runtime.submit(request).await {
            Ok(result) => {
                let tools_called = self.tools_called_since(receipts_before);
                tracing::info!(
                    session_id = %session_id.0,
                    receipt_id = %result.receipt_id.0,
                    tools = tools_called.len(),
                    "chat turn completed"
                );
                Ok(ChatTurnOutcome {
                    session_id,
                    reply: result.response.content,
                    tokens_in: result.cost.tokens_in,
                    tokens_out: result.cost.tokens_out,
                    cents: result.cost.cents,
                    tools_called,
                    receipt_id: result.receipt_id.0.to_string(),
                })
            }
            Err(e) => {
                tracing::warn!(session_id = %session_id.0, error = %e, "chat turn failed");
                Err(e)
            }
        };
        // A dropped receiver means the HTTP caller's request future was cancelled
        // (client hung up); nothing to do but discard the outcome.
        let _ = reply.send(outcome);
    }

    /// The number of receipts currently persisted in the chain log (`0` if the
    /// log is absent or unreadable). Brackets a turn's receipts for
    /// [`tools_called_since`](Self::tools_called_since).
    fn receipt_count(&self) -> usize {
        load_persisted_chain(&self.receipt_log)
            .map(|chain| chain.len())
            .unwrap_or(0)
    }

    /// The tool names recorded on every receipt appended after index `before` —
    /// the tools this turn's provider iterations invoked, in receipt order.
    fn tools_called_since(&self, before: usize) -> Vec<String> {
        match load_persisted_chain(&self.receipt_log) {
            Ok(chain) => chain
                .into_iter()
                .skip(before)
                .flat_map(|receipt| {
                    receipt
                        .body
                        .tool_calls
                        .into_iter()
                        .map(|call| call.tool_name)
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "reading receipt chain for tools_called");
                Vec::new()
            }
        }
    }

    /// Post `text` to `channel`, routing to the backend the turn originated on.
    /// Returns the provider's message id on success.
    async fn post_reply(
        &self,
        origin: Origin,
        channel: &str,
        text: &str,
    ) -> anyhow::Result<String> {
        match origin {
            Origin::Slack => self
                .slack
                .post_message(channel, text, None)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string())),
            Origin::Matrix => {
                let matrix = self.matrix.get().ok_or_else(|| {
                    anyhow::anyhow!("matrix reply requested but no channel attached")
                })?;
                matrix
                    .send_text(channel, text)
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
            }
            Origin::Discord => {
                let discord = self.discord.get().ok_or_else(|| {
                    anyhow::anyhow!("discord reply requested but no channel attached")
                })?;
                discord
                    .send_text(channel, text)
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
            }
            Origin::Telegram => {
                let telegram = self.telegram.get().ok_or_else(|| {
                    anyhow::anyhow!("telegram reply requested but no channel attached")
                })?;
                telegram
                    .send_text(channel, text)
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
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
                tool_allowlist: self.tool_allowlist.clone(),
            },
        )?;
        Ok(token.to_base64()?)
    }
}

/// Spawn the worker thread: a current-thread Tokio runtime that drains the work
/// queue, processing each message to completion in arrival order. Returns the
/// sender the HTTP layer enqueues onto.
fn spawn_worker(
    processor: Processor,
) -> (mpsc::UnboundedSender<WorkItem>, std::thread::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<WorkItem>();
    let handle = std::thread::Builder::new()
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
                while let Some(item) = rx.recv().await {
                    match item {
                        WorkItem::Channel(message) => processor.handle(message).await,
                        WorkItem::Http(turn) => processor.handle_http(turn).await,
                    }
                }
            });
        })
        .expect("spawning the ardur-turn-worker thread");
    (tx, handle)
}

/// Spawn a task that drains inbound messages from a channel adapter onto the
/// turn-worker queue — the same path a verified Slack message takes, so the
/// fused turn runs identically regardless of origin. The loop ends when the
/// worker is gone or the channel's `receive` errors. `label` names the channel
/// in log lines.
fn spawn_inbound_forwarder<G>(
    label: &'static str,
    work_tx: Arc<Mutex<Option<mpsc::UnboundedSender<WorkItem>>>>,
    channel: Arc<G>,
) where
    G: MessagingGateway + Send + Sync + 'static,
{
    tokio::spawn(async move {
        loop {
            match channel.receive().await {
                Ok(incoming) => {
                    let Some(tx) = work_tx
                        .lock()
                        .expect("work_tx mutex poisoned")
                        .as_ref()
                        .cloned()
                    else {
                        tracing::info!(
                            channel = label,
                            "turn worker is shutting down; stopping forwarder"
                        );
                        break;
                    };
                    if tx.send(WorkItem::Channel(incoming)).is_err() {
                        tracing::error!(channel = label, "turn worker is gone; stopping forwarder");
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!(channel = label, error = %e, "channel receive failed; stopping forwarder");
                    break;
                }
            }
        }
    });
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
/// The Qdrant connection config shared by the `qdrant` and `hybrid` backends:
/// collection/dim/api-key defaults come from `ardur-memory-qdrant`, with URL and
/// collection overridden from the validated [`Config`] when set.
fn qdrant_config(config: &Config) -> QdrantMemoryConfig {
    let mut qcfg = QdrantMemoryConfig::from_env();
    if let Some(url) = &config.qdrant_url {
        qcfg = qcfg.with_url(url.clone());
    }
    if let Some(collection) = &config.qdrant_collection {
        qcfg = qcfg.with_collection_name(collection.clone());
    }
    qcfg
}

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

/// Compile the Cedar policy: a configured operator file must exist and compile.
/// Without a configured path, production uses deny-all; tests/lab boots may opt
/// into the embedded permissive policy with an explicit dev flag.
fn load_policy(path: Option<&Path>, dev_permissive: bool) -> anyhow::Result<CedarPolicyBundle> {
    let source = match path {
        Some(p) if p.exists() => PolicySource::File(p.to_path_buf()),
        Some(p) => {
            return Err(anyhow::anyhow!(
                "configured Cedar policy path does not exist: {}",
                p.display()
            ));
        }
        None if dev_permissive => PolicySource::Embedded(DEFAULT_POLICY.to_string()),
        None => PolicySource::Embedded(DENY_ALL_POLICY.to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_session_journals::InMemorySessionJournal;

    #[test]
    fn shutdown_closes_worker_queue_and_joins_thread() {
        let (work_tx, mut work_rx) = mpsc::unbounded_channel::<WorkItem>();
        let (joined_tx, joined_rx) = std::sync::mpsc::channel();
        let worker_handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime builds");
            runtime.block_on(async move { while work_rx.recv().await.is_some() {} });
            joined_tx.send(()).expect("joined signal sends");
        });

        let tempdir = tempfile::tempdir().expect("tempdir");
        let state = AppState {
            slack: Arc::new(SlackAdapter::new(
                SecretString::from("xoxb-test".to_string()),
                SecretString::from("signing-secret".to_string()),
                "A123".to_string(),
            )),
            work_tx: Arc::new(Mutex::new(Some(work_tx))),
            worker_handle: Mutex::new(Some(worker_handle)),
            journal: Arc::new(InMemorySessionJournal::new(SessionId::new())),
            data_dir: tempdir.path().to_path_buf(),
            chat_bearer_tokens: Vec::new(),
            admin_bearer_tokens: Vec::new(),
            tool_allowlist: Vec::new(),
            cost_budget_cents: 0,
            mcp: None,
            matrix: Arc::new(OnceLock::new()),
            discord: Arc::new(OnceLock::new()),
            telegram: Arc::new(OnceLock::new()),
        };

        assert!(state.worker_alive());
        state.shutdown();
        assert!(!state.worker_alive());
        joined_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("worker thread observed channel close");
    }
}
