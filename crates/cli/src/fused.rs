//! [`FusedEngine`] — the FusedRuntime-backed substrate a `ardur chat` session
//! drives by default.
//!
//! Where the legacy [`ChatEngine`](crate::ChatEngine) echoes through the §1.0
//! `InMemoryRuntime` (no provider, no receipt, no journal), this engine wires
//! the §11.x Phase-2 [`FusedRuntime`]: one [`FusedRuntime::submit`] per turn
//! runs the full ten-stage pipeline — cap-token verify, Cedar authorization,
//! cost admission, provider dispatch, signed-and-chained receipt, cost
//! finalize, memory write, and a durable session journal.
//!
//! # Construction
//!
//! [`FusedEngine::new`] resolves the provider, loads (or mints) the persistent
//! keys and policies under `~/.ardur/`, mints a per-session cap-token, and
//! builds a [`FusedRuntime`] over file-backed receipts + journals:
//!
//! - **Provider** — selected by `ARDUR_PROVIDER` (default `anthropic`) via
//!   [`ardur_provider_selector::from_env`]: `anthropic` | `openrouter` |
//!   `ollama` | `codex`. When the selected backend cannot be built from the
//!   environment (a credentialed backend with no key), the engine falls back to
//!   [`AnthropicProvider::stub`] and reports [`offline`](FusedEngine::offline)
//!   so the REPL can print an offline notice. An unknown `ARDUR_PROVIDER` value
//!   panics at boot.
//! - **Budget** — the session holder is provisioned with `budget_cents` on the
//!   cents axis (and generously on the token/wall/attention axes), and each turn
//!   reserves a per-turn ceiling (`ARDUR_CLI_PER_TURN_CENTS`, default
//!   `min(budget_cents, 100)`) so a session of many turns depletes the budget
//!   gracefully rather than reserving it all on turn one.
//! - **Cap-token** — minted once at session start for subject
//!   `cli://localhost-<uid>`, audience `cli`, tool `chat.submit`, expiring one
//!   hour from process start.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ardur_cap_token::{CapScope, CapTokenIssuer, HolderId as CapHolderId, VerifiedClaims};
use ardur_cedar_policy::CedarPolicyBundle;
use ardur_cost_gate::{CostEnvelope, CostTuple as GateCostTuple, HolderId as GateHolderId};
use ardur_fused_runtime::FusedRuntimeBuilder;
use ardur_memory::{
    HolderId as MemoryHolderId, InMemoryMemoryRuntime, MemoryCard, MemoryControlPlane, ReceiptId,
    RecordId, UnixTsMillis,
};
use ardur_provider_runtime::{
    AnthropicProvider, CompletionRequest, InstrumentedProvider, ModelId, Provider,
};
use ardur_provider_selector as provider_selector;
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};
use ardur_session_journals::FileSessionJournal;

use crate::config::Config;
use crate::engine::TurnOutcome;
use crate::error::CliError;
use crate::state::StateDirs;
use crate::stream::{StreamOutcome, drive_turn};

/// The audience the session cap-token is scoped to (matches the runtime's
/// verifier caveat).
const AUDIENCE: &str = "cli";
/// The tool/capability every chat turn exercises.
const TOOL: &str = "chat.submit";
/// The session cap-token's lifetime, in seconds (one hour from process start).
const CAP_TTL_SECS: u64 = 3_600;
/// The default per-turn cents ceiling when `ARDUR_CLI_PER_TURN_CENTS` is unset,
/// capped at the session budget so a tiny budget still affords a turn.
const DEFAULT_PER_TURN_CENTS: u64 = 100;
/// The output-token ceiling on a streamed turn's request, matching the fused
/// runtime's own default (`FusedRuntimeBuilder`'s `max_tokens`).
const STREAM_MAX_TOKENS: u32 = 1024;

/// A FusedRuntime-backed chat substrate for one interactive session.
pub struct FusedEngine {
    runtime: ardur_fused_runtime::FusedRuntime,
    /// The selected (instrumented) backend, retained so [`stream_turn`] can call
    /// [`Provider::stream`] directly at the CLI layer (the §2.1b streaming path
    /// bypasses the fused pipeline — see [`crate::stream`]).
    ///
    /// [`stream_turn`]: FusedEngine::stream_turn
    /// [`Provider::stream`]: ardur_provider_runtime::Provider::stream
    provider: Arc<dyn Provider>,
    /// The model streamed requests are built against (the same the fused runtime
    /// dispatches against).
    model: ModelId,
    cap_token: CapTokenRef,
    holder: GateHolderId,
    policies: CedarPolicyBundle,
    memory: Arc<InMemoryMemoryRuntime>,
    session_id: SessionId,
    remaining: Arc<AtomicU64>,
    offline: bool,
}

impl FusedEngine {
    /// Wire a fresh engine: resolve the provider, load/mint the persistent keys
    /// and Cedar policies, mint the session cap-token, and build the fused
    /// runtime over file-backed receipts + journals.
    pub fn new(config: &Config, dirs: &StateDirs, budget_cents: u64) -> Result<Self, CliError> {
        let model = ModelId::new(&config.model);

        // Select the live backend via `ARDUR_PROVIDER` (default `anthropic`). An
        // unknown selector panics at boot inside the selector — a typo aborts
        // loudly rather than silently downgrading. A *valid* selection whose
        // credentials are missing (e.g. no `ANTHROPIC_API_KEY` /
        // `OPENROUTER_API_KEY`) falls back to the network-free Anthropic stub and
        // flags the session offline; the credential-free backends (ollama, codex)
        // never take this branch.
        let (provider, offline): (Arc<dyn Provider>, bool) =
            match provider_selector::from_env(model.clone()) {
                Ok(live) => {
                    tracing::info!(provider = %live.id().0, "using provider");
                    (live, false)
                }
                Err(_) => {
                    let stub: Arc<dyn Provider> = Arc::new(AnthropicProvider::stub(model.clone()));
                    tracing::info!(
                        provider = %stub.id().0,
                        offline = true,
                        "selected provider unavailable; using offline stub"
                    );
                    (stub, true)
                }
            };

        // Instrument the selected provider so each dispatch emits a `provider.send`
        // span carrying the OpenTelemetry GenAI semconv attributes; those export to
        // an OTLP backend when `ARDUR_OTEL_ENABLED=true`, and otherwise route to the
        // CLI's console subscriber.
        let provider = InstrumentedProvider::wrap(provider);
        // Keep a handle to the same instrumented provider the runtime owns so the
        // streaming REPL path can drive `Provider::stream` directly.
        let provider_handle = Arc::clone(&provider);

        let issuer = dirs.load_or_create_issuer()?;
        let cap_root = issuer.public_key();
        let receipt_key = dirs.load_or_create_receipt_key()?;
        let policies = dirs.load_cedar_policies()?;

        let subject = dirs.local_subject();
        let holder = GateHolderId(subject.clone());

        // Mint the per-session cap-token, anchored at process start.
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cap = issuer
            .issue(
                CapHolderId(subject.clone()),
                CapScope {
                    audience: AUDIENCE.to_string(),
                    expires_unix: now_unix + CAP_TTL_SECS,
                    // The verifier checks the per-turn cost (1 unit) against this
                    // ceiling, so it must be at least 1.
                    budget_remaining: budget_cents.max(1),
                    tool_allowlist: vec![
                        TOOL.to_string(),
                        ardur_memory::MEMORY_READ_CAPABILITY.to_string(),
                        ardur_memory::MEMORY_WRITE_CAPABILITY.to_string(),
                    ],
                },
            )
            .map_err(|e| CliError::State(format!("minting the session cap-token: {e}")))?;
        let cap_token = CapTokenRef(
            cap.to_base64()
                .map_err(|e| CliError::State(format!("serializing the session cap-token: {e}")))?,
        );

        // Per-turn cost ceiling: only the cents axis gates (token/wall/attention
        // maxes are zero), so the budget depletes one turn's spend at a time.
        let per_turn_cents = per_turn_cents(budget_cents);
        let envelope = CostEnvelope {
            tokens_in_max: 0,
            tokens_out_max: 0,
            cents_max: u32::try_from(per_turn_cents).unwrap_or(u32::MAX),
            wall_ms_max: 0,
            attention_score_max: 0,
        };

        let session_id = SessionId::new();
        let journal = FileSessionJournal::new(&dirs.journals, session_id)
            .map_err(|e| CliError::State(format!("opening the session journal: {e}")))?;
        // TODO §7.0: no file-backed `MemoryRuntime` exists yet, so the bi-temporal
        // memory sink is in-process for now (the `~/.ardur/memory/` dir is created
        // for the persistent store that replaces this).
        let memory = Arc::new(InMemoryMemoryRuntime::new());

        let runtime = FusedRuntimeBuilder::new(
            cap_root,
            policies.clone(),
            provider,
            receipt_key,
            model.clone(),
        )
        .audience(AUDIENCE)
        .tool(TOOL)
        .provision_budget(
            holder.clone(),
            GateCostTuple {
                tokens_in: 1_000_000_000,
                tokens_out: 1_000_000_000,
                cents: budget_cents,
                wall_ms: 1_000_000_000,
                attention_score: 1_000_000_000,
            },
        )
        .projected_envelope(envelope)
        .with_memory(memory.clone())
        .with_journal(Arc::new(journal))
        .receipt_log(dirs.receipt_log())
        .build()
        .map_err(|e| CliError::State(format!("building the fused runtime: {e}")))?;

        Ok(Self {
            runtime,
            provider: provider_handle,
            model,
            cap_token,
            holder,
            policies,
            memory,
            session_id,
            remaining: Arc::new(AtomicU64::new(budget_cents)),
            offline,
        })
    }

    /// Whether this session fell back to the network-free stub provider (no
    /// `ANTHROPIC_API_KEY`).
    #[must_use]
    pub fn offline(&self) -> bool {
        self.offline
    }

    /// A shared handle to the session's remaining-cents counter — read by the
    /// prompt indicator and the `/budget` command.
    #[must_use]
    pub fn budget_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.remaining)
    }

    /// The session's remaining budget, in cents.
    #[must_use]
    pub fn remaining_cents(&self) -> u64 {
        self.remaining.load(Ordering::SeqCst)
    }

    /// Whether the selected backend can stream tokens incrementally.
    #[must_use]
    pub fn supports_streaming(&self) -> bool {
        self.provider.supports_streaming()
    }

    /// Whether the REPL should drive this turn through the progressive
    /// [`stream_turn`](Self::stream_turn) path rather than the fused
    /// [`run_turn`](Self::run_turn).
    ///
    /// True only for a **live** streaming-capable backend. The offline stub is
    /// excluded: it has no incremental SSE feed (its `stream()` is the default
    /// wrap-`complete()` impl, so streaming buys no UX), and routing it through
    /// the fused pipeline keeps the offline session's signed receipts and durable
    /// journal — the very substrate guarantees the offline mode demonstrates.
    #[must_use]
    pub fn should_stream(&self) -> bool {
        !self.offline && self.provider.supports_streaming()
    }

    /// Run a `/memory ...` explorer command against this session's scoped memory.
    #[must_use]
    pub fn memory_command(&self, args: &str) -> String {
        let Some(capability) = memory_command_capability(args) else {
            return "usage: /memory list [--json] | /memory show <id> | /memory forget <id>"
                .to_string();
        };
        let claims = match self
            .runtime
            .verify_cap_token_for_tool(&self.cap_token, capability)
        {
            Ok(claims) => claims,
            Err(e) => return format!("memory authorization denied: {e}"),
        };
        Self::memory_command_on(
            &self.memory,
            &self.policies,
            &claims,
            &self.holder.0,
            args,
            now_ms(),
        )
    }

    /// Pure helper for the CLI memory explorer. Kept public so integration tests
    /// can exercise CRUD, workspace isolation, export formatting, and denial
    /// paths without booting providers or touching `~/.ardur`.
    #[must_use]
    pub fn memory_command_on(
        memory: &Arc<InMemoryMemoryRuntime>,
        policies: &CedarPolicyBundle,
        claims: &VerifiedClaims,
        subject: &str,
        args: &str,
        now_ms: u64,
    ) -> String {
        let mut parts = args.split_whitespace();
        let command = parts.next().unwrap_or("list");
        let rest: Vec<&str> = parts.collect();
        let holder = MemoryHolderId(subject.to_string());
        let plane = MemoryControlPlane::new(memory.as_ref(), policies.clone());
        match command {
            "" | "list" => {
                let cards = match plane.list(claims, &holder, UnixTsMillis(now_ms)) {
                    Ok(cards) => cards,
                    Err(e) => return format!("memory list denied: {e}"),
                };
                if rest
                    .iter()
                    .any(|arg| matches!(*arg, "--json" | "--export=json"))
                {
                    return serde_json::to_string_pretty(&cards)
                        .unwrap_or_else(|e| format!("memory export error: {e}"));
                }
                if cards.is_empty() {
                    return "memory: no current cards".to_string();
                }
                cards
                    .iter()
                    .map(memory_card_line)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "show" => {
                let Some(id) = rest.first().and_then(|raw| uuid::Uuid::parse_str(raw).ok()) else {
                    return "usage: /memory show <id>".to_string();
                };
                let rec =
                    match plane.show_as_of(claims, &holder, RecordId(id), UnixTsMillis(now_ms)) {
                        Ok(Some(rec)) => rec,
                        Ok(None) => return format!("memory {id} not found"),
                        Err(e) => return format!("memory show denied: {e}"),
                    };
                serde_json::to_string_pretty(&rec)
                    .unwrap_or_else(|e| format!("memory show error: {e}"))
            }
            "forget" => {
                let Some(id) = rest.first().and_then(|raw| uuid::Uuid::parse_str(raw).ok()) else {
                    return "usage: /memory forget <id>".to_string();
                };
                match plane.forget(
                    claims,
                    &holder,
                    RecordId(id),
                    UnixTsMillis(now_ms),
                    ReceiptId(uuid::Uuid::new_v4()),
                ) {
                    Ok(()) => format!("forgot memory {id}"),
                    Err(e) => format!("memory forget denied: {e}"),
                }
            }
            _ => {
                "usage: /memory list [--json] | /memory show <id> | /memory forget <id>".to_string()
            }
        }
    }

    /// Run one chat turn by streaming the provider **directly**, rendering tokens
    /// progressively to `out`.
    ///
    /// This is the §2.1b interactive path: it calls
    /// [`Provider::stream`](ardur_provider_runtime::Provider::stream) at the CLI
    /// layer, **bypassing** the fused runtime's ten-stage pipeline (cap-token
    /// verify, Cedar authorization, cost admission, signed receipt, durable
    /// journal) that [`run_turn`](Self::run_turn) routes through. The displayed
    /// budget is decremented locally from the streamed turn's usage cost so the
    /// prompt stays sensible; a subsequent fused turn re-syncs the balance from
    /// the gate ledger. Threading streaming *through* the fused runtime is the
    /// proposed follow-up.
    ///
    /// The REPL only routes here once it has confirmed streaming is enabled
    /// (`--no-stream` absent) *and* [`supports_streaming`](Self::supports_streaming),
    /// so this always drives the real streaming path; a non-streaming-capable
    /// backend keeps the full-pipeline [`run_turn`](Self::run_turn) instead.
    pub async fn stream_turn<W: std::io::Write>(
        &self,
        messages: &[ChatMessage],
        out: &mut W,
        ctx: &crate::stream::RenderCtx<'_>,
    ) -> std::io::Result<StreamOutcome> {
        let req = CompletionRequest::new(messages.to_vec(), self.model.clone(), STREAM_MAX_TOKENS)
            .streaming();
        let outcome = drive_turn(self.provider.as_ref(), req, true, out, ctx).await?;

        // Decrement the displayed balance by this turn's usage cost. The gate
        // ledger is untouched on this bypass path (documented trade-off above).
        if let Some(usage) = outcome.usage {
            let used = self.provider.rate_card().price(usage).cents;
            self.remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |bal| {
                    Some(bal.saturating_sub(used))
                })
                .ok();
        }
        Ok(outcome)
    }

    /// Run one chat turn over `messages` through the full fused pipeline, then
    /// refresh the displayed budget from the cost gate's ledger.
    pub async fn run_turn(&self, messages: &[ChatMessage]) -> Result<TurnOutcome, CliError> {
        let result = self
            .runtime
            .submit(SubmitRequest {
                messages: messages.to_vec(),
                cap_token: self.cap_token.clone(),
                session_id: self.session_id,
                requested_provider: None,
            })
            .await?;

        let used_cents = result.cost.cents;
        // Refresh the displayed balance from the same ledger the gate settled
        // against; fall back to a local decrement if the holder read fails.
        let remaining_cents = match self.runtime.remaining_budget(&self.holder).await {
            Some(balance) => balance.cents,
            None => self.remaining_cents().saturating_sub(used_cents),
        };
        self.remaining.store(remaining_cents, Ordering::SeqCst);

        Ok(TurnOutcome {
            response: result.response.content,
            used_cents,
            remaining_cents,
        })
    }
}

fn memory_command_capability(args: &str) -> Option<&'static str> {
    match args.split_whitespace().next().unwrap_or("list") {
        "" | "list" | "show" => Some(ardur_memory::MEMORY_READ_CAPABILITY),
        "forget" => Some(ardur_memory::MEMORY_WRITE_CAPABILITY),
        _ => None,
    }
}

fn memory_card_line(card: &MemoryCard) -> String {
    let receipt = card
        .receipt_id
        .map(|r| r.0.to_string())
        .unwrap_or_else(|| "unreceipted".to_string());
    let source = card.source.as_deref().unwrap_or("unknown");
    let scope = card.scope.as_deref().unwrap_or(card.subject.0.as_str());
    let confidence = card
        .confidence
        .map(|c| format!("{c:.2}"))
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "{} source={} scope={} confidence={} receipt={} valid_from={} {}",
        card.record_id,
        source,
        scope,
        confidence,
        receipt,
        card.valid_from.0,
        memory_payload_text(&card.payload)
    )
}

fn memory_payload_text(payload: &serde_json::Value) -> String {
    if let Some(object) = payload.get("object") {
        return match object {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    match payload {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The per-turn cents ceiling: `ARDUR_CLI_PER_TURN_CENTS` if set and parseable,
/// else `min(budget_cents, 100)`, clamped to at least 1.
fn per_turn_cents(budget_cents: u64) -> u64 {
    std::env::var("ARDUR_CLI_PER_TURN_CENTS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or_else(|| budget_cents.min(DEFAULT_PER_TURN_CENTS))
        .max(1)
}
