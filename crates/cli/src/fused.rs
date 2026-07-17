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
//!   `openai-compat` | `ollama` | `codex` | `claude-cli`. When the selected
//!   backend cannot be built from the environment (a credentialed backend with
//!   no key), the engine falls back to [`AnthropicProvider::stub`] and reports
//!   [`offline`](FusedEngine::offline) so the REPL can print an offline notice.
//!   An unknown `ARDUR_PROVIDER` value returns a typed provider-selection error
//!   so operators get a clean typo message instead of a silent fallback.
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
    AnthropicProvider, InstrumentedProvider, ModelId, Provider, ProviderError,
};
use ardur_provider_selector as provider_selector;
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};
use ardur_session_journals::FileSessionJournal;

use crate::config::Config;
use crate::engine::TurnOutcome;
use crate::error::CliError;
use crate::state::StateDirs;
use crate::stream::{StreamOutcome, drive_fused_turn};

/// The audience the session cap-token is scoped to (matches the runtime's
/// verifier caveat).
const AUDIENCE: &str = "cli";
/// The tool/capability every chat turn exercises.
const TOOL: &str = "chat.submit";
/// §1.8 — the capability `/checkpoint` and `/rollback` exercise.
const SESSION_CHECKPOINT_CAPABILITY: &str = "session.checkpoint";
/// §1.8 — the capability `/rollback` exercises to roll back to a checkpoint.
const SESSION_ROLLBACK_CAPABILITY: &str = "session.rollback";
/// §1.7 — the capability `/compact` and `/compact preview` exercise.
const CONTEXT_COMPACT_CAPABILITY: &str = "context.compact";
/// §1.9 — the capability `/background`/`/bg`/`/btw` and `/task cancel` exercise.
const BACKGROUND_TASK_CAPABILITY: &str = "task.background";
/// §1.10 — the capability `/steer`/`/tell` exercise.
const STEER_CAPABILITY: &str = "input.steer";
/// §1.10 — the capability `/interrupt` exercises.
const INTERRUPT_CAPABILITY: &str = "input.interrupt";
/// The session cap-token's lifetime, in seconds (one hour from process start).
const CAP_TTL_SECS: u64 = 3_600;
/// The default per-turn cents ceiling when `ARDUR_CLI_PER_TURN_CENTS` is unset,
/// capped at the session budget so a tiny budget still affords a turn.
const DEFAULT_PER_TURN_CENTS: u64 = 100;
/// A FusedRuntime-backed chat substrate for one interactive session.
pub struct FusedEngine {
    runtime: ardur_fused_runtime::FusedRuntime,
    /// The selected (instrumented) backend, retained for streaming capability
    /// discovery and rate-card rendering. Streamed turns themselves run through
    /// [`ardur_fused_runtime::FusedRuntime::stream`].
    provider: Arc<dyn Provider>,
    cap_token: CapTokenRef,
    holder: GateHolderId,
    policies: CedarPolicyBundle,
    memory: Arc<InMemoryMemoryRuntime>,
    session_id: SessionId,
    remaining: Arc<AtomicU64>,
    offline: bool,
}

impl FusedEngine {
    /// Wire a fresh engine over a newly-minted session id.
    ///
    /// Resolves the provider, loads/mints persistent keys and Cedar policies,
    /// mints the session cap-token, and builds the fused runtime over
    /// file-backed receipts + journals.
    pub async fn new(
        config: &Config,
        dirs: &StateDirs,
        budget_cents: u64,
    ) -> Result<Self, CliError> {
        Self::new_for_session(config, dirs, budget_cents, None).await
    }

    /// Wire an engine over a specific session id, or mint a fresh one when absent.
    ///
    /// Supplying `session_id` reopens that session's file-backed journal so new
    /// turns append to the existing transcript instead of starting a new log.
    pub async fn new_for_session(
        config: &Config,
        dirs: &StateDirs,
        budget_cents: u64,
        session_id: Option<SessionId>,
    ) -> Result<Self, CliError> {
        let model = ModelId::new(&config.model);

        // Select the live backend via `ARDUR_PROVIDER` (default `anthropic`).
        // Invalid selectors are operator typos and must abort cleanly. A *valid*
        // selection whose credentials are missing (e.g. no `ANTHROPIC_API_KEY` /
        // `OPENROUTER_API_KEY`) falls back to the network-free Anthropic stub and
        // flags the session offline; credential-free backends do not take this
        // branch.
        let (provider, offline): (Arc<dyn Provider>, bool) =
            match provider_selector::from_env(model.clone()) {
                Ok(live) => {
                    tracing::info!(provider = %live.id().0, "using provider");
                    (live, false)
                }
                Err(e @ ProviderError::InvalidSelection(_)) => return Err(CliError::Provider(e)),
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
        // Keep a handle to the same instrumented provider the runtime owns for
        // streaming capability discovery and rate-card rendering.
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
                        SESSION_CHECKPOINT_CAPABILITY.to_string(),
                        SESSION_ROLLBACK_CAPABILITY.to_string(),
                        CONTEXT_COMPACT_CAPABILITY.to_string(),
                        BACKGROUND_TASK_CAPABILITY.to_string(),
                        STEER_CAPABILITY.to_string(),
                        INTERRUPT_CAPABILITY.to_string(),
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

        let session_id = session_id.unwrap_or_default();
        let journal = FileSessionJournal::new(&dirs.journals, session_id)
            .map_err(|e| CliError::State(format!("opening the session journal: {e}")))?;
        // TODO §7.0: no file-backed `MemoryRuntime` exists yet, so the bi-temporal
        // memory sink is in-process for now (the `~/.ardur/memory/` dir is created
        // for the persistent store that replaces this).
        let memory = Arc::new(InMemoryMemoryRuntime::new());

        let (runtime, reconciliation) = FusedRuntimeBuilder::new(
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
        // ARD-H1: install the built-in injection-defense signatures so `ardur
        // chat` scans prompts too, rather than shipping stage 4.5 inert.
        .with_default_injection_filters()
        .receipt_log(dirs.receipt_log())
        .build_reconciled()
        .await
        .map_err(|e| CliError::State(format!("building/reconciling the fused runtime: {e}")))?;
        if reconciliation.orphan_receipt_count() > 0 {
            tracing::warn!(
                repaired = reconciliation.orphan_receipt_count(),
                action = ?reconciliation.action,
                "reconciled orphan receipts during CLI startup"
            );
        }

        dirs.record_session_metadata(
            &session_id.0.to_string(),
            &provider_handle.id().0,
            &model.0,
            "cli",
        )?;

        Ok(Self {
            runtime,
            provider: provider_handle,
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

    /// **§1.8.** Record a checkpoint over the session's current history.
    ///
    /// # Errors
    /// Returns [`CliError`] if the session cap-token does not grant the
    /// checkpoint capability, no journal is configured, or the journal
    /// append fails.
    pub async fn checkpoint(
        &self,
        label: Option<String>,
    ) -> Result<ardur_fused_runtime::CheckpointOutcome, CliError> {
        self.runtime
            .checkpoint(
                self.session_id,
                &self.cap_token,
                SESSION_CHECKPOINT_CAPABILITY,
                label,
            )
            .await
            .map_err(|e| CliError::State(format!("checkpoint failed: {e}")))
    }

    /// **§1.8.** List every checkpoint recorded in this session so far.
    ///
    /// # Errors
    /// Returns [`CliError`] if no journal is configured or the replay fails.
    pub async fn list_checkpoints(
        &self,
    ) -> Result<Vec<ardur_fused_runtime::CheckpointInfo>, CliError> {
        self.runtime
            .list_checkpoints(self.session_id)
            .await
            .map_err(|e| CliError::State(format!("listing checkpoints failed: {e}")))
    }

    /// **§1.8.** Roll back the session to a previously recorded checkpoint.
    /// Returns the rollback outcome (the receipt + journal marker minted for
    /// it) alongside the rebuilt in-memory chat history the caller should
    /// replace its own `history: Vec<ChatMessage>` with.
    ///
    /// # Errors
    /// Returns [`CliError`] if the session cap-token does not grant the
    /// rollback capability, no journal is configured, `checkpoint_id` does
    /// not name a checkpoint in this session, or the journal append fails.
    pub async fn rollback(
        &self,
        checkpoint_id: uuid::Uuid,
    ) -> Result<(ardur_fused_runtime::RollbackOutcome, Vec<ChatMessage>), CliError> {
        let outcome = self
            .runtime
            .rollback_to_checkpoint(
                self.session_id,
                &self.cap_token,
                SESSION_ROLLBACK_CAPABILITY,
                checkpoint_id,
            )
            .await
            .map_err(|e| CliError::State(format!("rollback failed: {e}")))?;
        let history = crate::journal_entries_to_history(&outcome.retained_entries);
        Ok((outcome, history))
    }

    /// **§1.7.** Summarize `history` and install the result as a compaction
    /// checkpoint (restorable later with [`rollback`](Self::rollback)).
    ///
    /// # Errors
    /// Returns [`CliError`] if the session cap-token does not grant the
    /// compact capability, the provider call fails, no journal is
    /// configured, or the journal append fails.
    pub async fn compact(
        &self,
        history: &[ChatMessage],
        focus: Option<String>,
    ) -> Result<ardur_fused_runtime::CompactOutcome, CliError> {
        self.runtime
            .compact(
                self.session_id,
                &self.cap_token,
                CONTEXT_COMPACT_CAPABILITY,
                history,
                focus,
            )
            .await
            .map_err(|e| CliError::State(format!("compact failed: {e}")))
    }

    /// **§1.7.** Preview a compaction candidate without installing it: no
    /// journal entry, no receipt.
    ///
    /// # Errors
    /// Returns [`CliError`] if the session cap-token does not grant the
    /// compact capability or the provider call fails.
    pub async fn preview_compact(
        &self,
        history: &[ChatMessage],
        focus: Option<String>,
    ) -> Result<String, CliError> {
        self.runtime
            .preview_compact(&self.cap_token, CONTEXT_COMPACT_CAPABILITY, history, focus)
            .await
            .map_err(|e| CliError::State(format!("compact preview failed: {e}")))
    }

    /// This engine's session id, so a caller (the §1.9 task registry) can
    /// tag a spawned background task's record with its owning session
    /// without needing its own copy threaded through separately.
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// **§1.9.** Run one agent background task's prompt to completion (or
    /// failure) and mint its terminal receipt. See
    /// [`ardur_fused_runtime::FusedRuntime::run_background_task`] for why a
    /// provider failure is `Ok` with `error` set rather than an `Err`.
    ///
    /// # Errors
    /// Returns [`CliError`] if the session cap-token does not grant the
    /// background-task capability or a receipt could not be minted.
    pub async fn run_background_task(
        &self,
        prompt: &str,
    ) -> Result<ardur_fused_runtime::BackgroundTaskOutcome, CliError> {
        self.runtime
            .run_background_task(
                self.session_id,
                &self.cap_token,
                BACKGROUND_TASK_CAPABILITY,
                prompt,
            )
            .await
            .map_err(|e| CliError::State(format!("background task failed: {e}")))
    }

    /// **§1.9.** Mint the terminal receipt for a background task the user
    /// explicitly cancelled.
    ///
    /// # Errors
    /// Returns [`CliError`] if the session cap-token does not grant the
    /// background-task capability or the receipt could not be minted.
    pub async fn cancel_background_task(&self) -> Result<ardur_runtime::ReceiptId, CliError> {
        self.runtime
            .cancel_background_task(self.session_id, &self.cap_token, BACKGROUND_TASK_CAPABILITY)
            .await
            .map_err(|e| CliError::State(format!("cancelling background task failed: {e}")))
    }

    /// **§1.10.** Mint the receipt for a steering directive accepted against
    /// `target_task_id`. See
    /// [`ardur_fused_runtime::FusedRuntime::accept_steer_directive`] for why
    /// this records evidence of the request without yet changing the
    /// target's in-flight behavior.
    ///
    /// # Errors
    /// Returns [`CliError`] if the session cap-token does not grant the
    /// steer capability or the receipt could not be minted.
    pub async fn accept_steer_directive(
        &self,
        target_task_id: uuid::Uuid,
        message: &str,
    ) -> Result<ardur_runtime::ReceiptId, CliError> {
        self.runtime
            .accept_steer_directive(
                self.session_id,
                &self.cap_token,
                STEER_CAPABILITY,
                target_task_id,
                message,
            )
            .await
            .map_err(|e| CliError::State(format!("steer failed: {e}")))
    }

    /// **§1.10.** Mint the receipt for an accepted interrupt against
    /// `target_task_id`.
    ///
    /// # Errors
    /// Returns [`CliError`] if the session cap-token does not grant the
    /// interrupt capability or the receipt could not be minted.
    pub async fn accept_interrupt(
        &self,
        target_task_id: uuid::Uuid,
    ) -> Result<ardur_runtime::ReceiptId, CliError> {
        self.runtime
            .accept_interrupt(
                self.session_id,
                &self.cap_token,
                INTERRUPT_CAPABILITY,
                target_task_id,
            )
            .await
            .map_err(|e| CliError::State(format!("interrupt failed: {e}")))
    }

    /// Run one progressive chat turn through the fused runtime's full ten-stage
    /// pipeline, rendering content events to `out` as they arrive.
    ///
    /// The same cap-token, Cedar policy, cost gate, receipt chain, memory plane,
    /// and durable session journal used by [`run_turn`](Self::run_turn) remain in
    /// force. Cancelling an unfinished stream leaves no receipt or journal entry.
    pub async fn stream_turn<W: std::io::Write>(
        &self,
        messages: &[ChatMessage],
        out: &mut W,
        ctx: &crate::stream::RenderCtx<'_>,
    ) -> std::io::Result<StreamOutcome> {
        let outcome = {
            let stream = self.runtime.stream(SubmitRequest {
                messages: messages.to_vec(),
                cap_token: self.cap_token.clone(),
                session_id: self.session_id,
                requested_provider: None,
            });
            drive_fused_turn(stream, out, ctx).await?
        };

        if let Some(balance) = self.runtime.remaining_budget(&self.holder).await {
            self.remaining.store(balance.cents, Ordering::SeqCst);
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
