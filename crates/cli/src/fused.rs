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
use ardur_fused_runtime::{ErMirrorEmitter, FusedRuntimeBuilder, GovernanceEmitter};
use ardur_memory::{
    HolderId as MemoryHolderId, InMemoryMemoryRuntime, MemoryCard, MemoryControlPlane, ReceiptId,
    RecordId, UnixTsMillis,
};
use ardur_provider_runtime::{
    AnthropicProvider, InstrumentedProvider, ModelId, Provider, ProviderError,
};
use ardur_provider_selector as provider_selector;
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};
use ardur_session_journals::{FileSessionJournal, SessionJournal};
use ardur_tool_registry::{
    HttpFetchTool, ListDirTool, ReadFileTool, ShellTool, ToolId, ToolRegistry, WriteFileTool,
};

use crate::config::Config;
use crate::engine::TurnOutcome;
use crate::error::CliError;
use crate::state::{GrantRecord, StateDirs, governance_mirror_enabled, read_grant_records};
use crate::stream::{StreamOutcome, drive_fused_turn};

/// The audience the session cap-token is scoped to (matches the runtime's
/// verifier caveat).
const AUDIENCE: &str = "cli";
/// #502 Seam B7: the stable `verifier_id` stamped into every governance ER the
/// CLI mirror mints — same scheme as the #560 fused-runtime contract, naming
/// the boot surface so an ER names its minting plane (CLI vs server) without
/// carrying host state.
const GOVERNANCE_VERIFIER_ID: &str = "spiffe://ardur/verifier/cli";
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

/// The tool side of the operator grant ledger (ARD-457): a [`ToolRegistry`]
/// holding every granted hardened built-in, plus the exact tool ids and `cap.*`
/// labels that must be minted into the session cap-token for those tools to be
/// invokable. Registration alone is dead-on-`CapDenied`; capability minting
/// alone points at tools that do not exist — both halves come from the same
/// pass here, mirroring the server's boot invariant.
///
/// Fail-closed mapping rules:
/// - `shell.run` is registered ONLY with scoped grants — scopes from every
///   scoped shell grant are unioned into one allowlist. A scope-less shell
///   grant is skipped with a warning — never the dev-only unrestricted shell.
///   NOTE: the shell allowlist is a PREFIX GATE, not a sandbox (the tool
///   executes through the system shell; `git; uname`-style chaining is not
///   confined by a prefix). Grants should name prefixes that are safe to run
///   with arbitrary arguments.
/// - `file.*` grants register ONLY with a scope (the confinement root), and
///   only the granted file tool ids register — granting `file.read` does not
///   expose `file.write`. Duplicate grants for the same file tool: last wins.
/// - `http.fetch` grants union their host scopes; a scope-less http grant
///   registers localhost-only (the strict default).
/// - Unknown tool ids in the ledger are skipped with a warning.
///
/// Tamper evidence: every record must (a) name this machine's local subject,
/// (b) carry a `receipt_id` found in the persisted receipt chain, and (c) have
/// that receipt's payload digest match the recomputed canonical grant payload.
/// A ledger copied in, hand-edited, or restored without its chain activates
/// nothing.
struct GrantTooling {
    registry: ToolRegistry,
    extra_allowlist: Vec<String>,
    /// The final http.fetch host allowlist, kept for tests (the tool owns it
    /// after registration).
    #[cfg(test)]
    http_hosts: Vec<String>,
    /// Whether the merged grant set includes a scope-less (localhost-only)
    /// http grant; when true the tool is built with localhost admitted
    /// alongside the allowlist. Kept for tests.
    #[cfg(test)]
    http_localhost: bool,
}

impl GrantTooling {
    /// The canonical payload a grant receipt commits to — the exact field set
    /// `ardur grant allow` writes (receipt_id excluded). serde_json canonicalizes
    /// key order on both sides, so digests compare byte-stably.
    fn canonical_payload(record: &GrantRecord) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "tool": record.tool,
            "capabilities": record.capabilities,
            "scope": record.scope,
            "subject": record.subject,
            "granted_at_ms": record.granted_at_ms,
        }))
        .expect("grant payload serializes")
    }

    /// The ledger records that pass subject + receipt validation.
    fn validate_records(
        records: &[GrantRecord],
        local_subject: &str,
        chain: &[ardur_fused_runtime::PersistedReceipt],
    ) -> Vec<GrantRecord> {
        let mut valid = Vec::new();
        for record in records {
            if record.subject != local_subject {
                tracing::warn!(
                    tool = %record.tool,
                    "grant subject does not match this machine's local subject; skipping"
                );
                continue;
            }
            let Some(receipt_id) = record.receipt_id.as_deref() else {
                tracing::warn!(tool = %record.tool, "grant has no receipt id; skipping");
                continue;
            };
            let expected = ardur_receipt::Sha256Digest::of(&Self::canonical_payload(record));
            // The matching receipt must be a GRANT receipt — a turn receipt that
            // happens to carry the same payload digest is not authorization.
            let receipted = chain.iter().any(|r| {
                r.body.receipt_id.to_string() == receipt_id
                    && r.body.verb.as_str() == "tool.grant.allow.v1"
                    && r.body.payload_digest == expected
            });
            if receipted {
                valid.push(record.clone());
            } else {
                tracing::warn!(
                    tool = %record.tool,
                    "grant's receipt is missing from the chain or its payload digest does not match; skipping (ledger tampered or chain restored?)"
                );
            }
        }
        valid
    }

    /// Map validated grants to tools. Pure: takes pre-validated records.
    fn from_records(records: &[GrantRecord]) -> Self {
        let mut registry = ToolRegistry::new();
        let mut shell_scopes: Vec<String> = Vec::new();
        let mut http_hosts: Vec<String> = Vec::new();
        let mut http_granted = false;
        // A scope-less http grant means localhost-only; if a scoped grant joins
        // the union, localhost must be re-added explicitly (a non-empty
        // allowlist otherwise drops it).
        let mut http_localhost = false;
        // Per-file-tool root, so granting file.read never exposes file.write.
        let mut file_roots: std::collections::HashMap<&str, std::path::PathBuf> =
            std::collections::HashMap::new();
        let mut wanted: Vec<&GrantRecord> = Vec::new();

        for record in records {
            match record.tool.as_str() {
                "shell.run" => match record.scope.as_deref().map(str::trim) {
                    Some(scope) if !scope.is_empty() => {
                        shell_scopes.push(scope.to_string());
                        wanted.push(record);
                    }
                    _ => tracing::warn!(
                        tool = "shell.run",
                        "grant has no --scope; skipping (never registers the unrestricted shell)"
                    ),
                },
                "http.fetch" => {
                    http_granted = true;
                    match record.scope.as_deref().map(str::trim) {
                        // A scope-less grant means localhost-only — preserve that
                        // even when a scoped grant joins the union, because a
                        // non-empty allowlist otherwise drops localhost.
                        Some("") | None => http_localhost = true,
                        Some(scope) => http_hosts.extend(
                            scope
                                .split(',')
                                .map(str::trim)
                                .filter(|h| !h.is_empty())
                                .map(str::to_string),
                        ),
                    }
                    wanted.push(record);
                }
                tool @ ("file.read" | "file.write" | "file.list") => {
                    match record.scope.as_deref().map(str::trim) {
                        Some(root) if !root.is_empty() => {
                            if file_roots
                                .insert(tool, std::path::PathBuf::from(root))
                                .is_some()
                            {
                                tracing::warn!(
                                    tool,
                                    "duplicate file-tool grant: the latest grant's root wins"
                                );
                            }
                            wanted.push(record);
                        }
                        _ => tracing::warn!(
                            tool,
                            "grant has no --scope (root dir); skipping (file tools stay unregistered)"
                        ),
                    }
                }
                other => tracing::warn!(tool = other, "unknown tool in grant ledger; skipping"),
            }
        }

        if !shell_scopes.is_empty() {
            let tool = ShellTool::with_allowlist(shell_scopes);
            if let Err(e) = registry.register(Box::new(tool)) {
                tracing::warn!(error = %e, "shell.run registration failed");
            }
        }
        if http_granted {
            let tool = HttpFetchTool::new()
                .with_allowlist(http_hosts.clone())
                .with_localhost_allowed(http_localhost);
            if let Err(e) = registry.register(Box::new(tool)) {
                tracing::warn!(error = %e, "http.fetch registration failed");
            }
        }
        for (tool_id, root) in &file_roots {
            let registered = match *tool_id {
                "file.read" => registry.register(Box::new(ReadFileTool::with_root(root.clone()))),
                "file.write" => registry.register(Box::new(WriteFileTool::with_root(root.clone()))),
                _ => registry.register(Box::new(ListDirTool::with_root(root.clone()))),
            };
            if let Err(e) = registered {
                tracing::warn!(tool = tool_id, error = %e, "file tool registration failed");
            }
        }

        // Only capabilities whose tool ACTUALLY registered join the session
        // allowlist — the server's ARD-457 invariant, mirrored.
        let mut extra_allowlist = Vec::new();
        for record in wanted {
            if registry.get(&ToolId::new(&record.tool)).is_some() {
                tracing::info!(
                    tool = %record.tool,
                    "registered operator-granted tool; its capabilities mint into the session cap-token"
                );
                extra_allowlist.push(record.tool.clone());
                extra_allowlist.extend(record.capabilities.iter().cloned());
            }
        }
        extra_allowlist.sort();
        extra_allowlist.dedup();
        Self {
            registry,
            extra_allowlist,
            #[cfg(test)]
            http_hosts,
            #[cfg(test)]
            http_localhost,
        }
    }

    fn from_ledger(dirs: &StateDirs) -> Self {
        let records = match read_grant_records(dirs) {
            Ok(records) => records,
            Err(e) => {
                // A corrupt ledger must not block chat; the fail-closed default
                // (no grant tools) is the safe reading of unparseable intent.
                tracing::warn!(error = %e, "could not read the grant ledger; no grant tools active");
                return Self::from_records(&[]);
            }
        };
        if records.is_empty() {
            return Self::from_records(&[]);
        }
        let chain = match ardur_fused_runtime::load_persisted_chain(dirs.receipt_log()) {
            Ok(chain) => chain,
            Err(e) => {
                tracing::warn!(error = %e, "could not load the receipt chain to validate grants; no grant tools active");
                return Self::from_records(&[]);
            }
        };
        let valid = Self::validate_records(&records, &dirs.local_subject(), &chain);
        Self::from_records(&valid)
    }
}
/// The default per-turn cents ceiling when `ARDUR_CLI_PER_TURN_CENTS` is unset,
/// capped at the session budget so a tiny budget still affords a turn.
const DEFAULT_PER_TURN_CENTS: u64 = 100;
/// Retained outside cancellable turn futures. No asynchronous work runs in
/// Drop; callers must finish explicitly before journal/runtime teardown.
#[derive(Clone)]
pub(crate) struct SettlementLifecycle {
    supervisor: ardur_fused_runtime::settlement::SettlementSupervisor,
    journal: Arc<dyn SessionJournal>,
}
impl SettlementLifecycle {
    pub(crate) fn new(
        runtime: &ardur_fused_runtime::FusedRuntime,
        journal: Arc<dyn SessionJournal>,
    ) -> Self {
        Self {
            supervisor: runtime.settlement_supervisor(),
            journal,
        }
    }

    pub(crate) async fn drain(&self) -> Result<(), CliError> {
        let result = self.supervisor.drain_pending(self.journal.as_ref()).await;
        let status = self.supervisor.status();
        if let Err(error) = result {
            tracing::error!(%error, ?status, "settlement drain unresolved; owner retained");
            return Err(CliError::State(format!(
                "settlement drain unresolved: {error}"
            )));
        }
        if !status.turns.is_empty()
            || status.busy.is_some()
            || status.executing.is_some()
            || status.boot_problem.is_some()
        {
            tracing::error!(?status, "settlement remains unresolved; owner retained");
            return Err(CliError::State(
                "settlement remains unresolved; inspect retained supervisor".into(),
            ));
        }
        Ok(())
    }

    // Drain even when the original result failed; preserve that error's priority.
    pub(crate) async fn after<T, E>(
        &self,
        result: Result<T, E>,
        map: impl FnOnce(CliError) -> E,
    ) -> Result<T, E> {
        let drained = self.drain().await.map_err(map);
        result.and_then(|value| drained.map(|()| value))
    }

    pub(crate) async fn finish(&self) -> Result<(), CliError> {
        self.supervisor.stop_admission();
        self.drain().await?;
        self.supervisor
            .clone()
            .try_close()
            .map_err(|_| CliError::State("unsafe settlement shutdown; owner retained".into()))?;
        self.journal
            .close()
            .await
            .map_err(|e| CliError::State(format!("closing session journal: {e}")))
    }
}

/// A borrowed event source: callers cannot retain or detach its owner.
type FusedSource<'s> = std::pin::Pin<
    &'s mut (
                dyn futures::Stream<
        Item = Result<ardur_fused_runtime::FusedEvent, ardur_runtime::RuntimeError>,
    > + Send
                    + 's
            ),
>;

/// A FusedRuntime-backed chat substrate for one interactive session.
pub struct FusedEngine {
    runtime: ardur_fused_runtime::FusedRuntime,
    pub(crate) settlements: SettlementLifecycle,
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

        Self::finish_wiring(
            config,
            dirs,
            budget_cents,
            session_id,
            model,
            provider,
            offline,
        )
        .await
    }

    /// Complete engine wiring over an already-selected provider. gh#539: the
    /// receipt-boundary regression tests inject a nonzero-usage streaming
    /// provider through this seam — every other construction step is identical
    /// to the env-selected path of
    /// [`new_for_session`](Self::new_for_session).
    #[cfg(test)]
    pub(crate) async fn new_with_provider(
        config: &Config,
        dirs: &StateDirs,
        budget_cents: u64,
        provider: Arc<dyn Provider>,
    ) -> Result<Self, CliError> {
        let model = ModelId::new(&config.model);
        Self::finish_wiring(config, dirs, budget_cents, None, model, provider, false).await
    }

    /// The shared tail of both constructors: instrumenting the provider, loading
    /// keys/policies/grants, minting the session cap-token, provisioning the
    /// budget, building the reconciled runtime, and recording session metadata.
    /// `config` supplies the model name only at the call sites; the model is
    /// passed through `model` here.
    async fn finish_wiring(
        _config: &Config,
        dirs: &StateDirs,
        budget_cents: u64,
        session_id: Option<SessionId>,
        model: ModelId,
        provider: Arc<dyn Provider>,
        offline: bool,
    ) -> Result<Self, CliError> {
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
        // gh#533 review: migrate a byte-exact legacy starter policy before
        // loading, so an installation created before the ContextCompact/
        // TaskBackground actions existed is not silently policy-denied for
        // /compact and /background. Customized policies are untouched.
        if dirs.upgrade_legacy_starter_policy()? {
            tracing::info!(
                "migrated the generated starter Cedar policy to include the \
                 ContextCompact/TaskBackground actions"
            );
        }
        let policies = dirs.load_cedar_policies()?;

        // ARD-457: consume the operator grant ledger — the granted hardened
        // tools register into the session's tool registry and their tool ids +
        // cap.* labels mint into the session cap-token below. No ledger (or a
        // corrupt one) means no grant tools: fail-closed, as before.
        let grant_tooling = GrantTooling::from_ledger(dirs);

        // Integration tools (ARD-459). Registering an adapter's tools in the
        // doctor registry only tells an operator the configuration is sound; it
        // does not make the tools callable. They have to join the registry the
        // engine actually runs with, and their capabilities have to join the
        // session cap-token, or a configured integration is reported healthy
        // and then cannot be invoked.
        //
        // Mirrors the grant invariant above: a capability joins the allowlist
        // only if its tool actually registered.
        let integration_tooling = IntegrationTooling::from_config(dirs);

        // Fold the integration tools into the grant registry BEFORE the
        // cap-token is minted. Registration is what decides which capabilities
        // are legitimate — a capability is granted only if its tool actually
        // registered — so the allowlist cannot be read until this has run.
        // Reading it earlier yields an empty list and every integration call is
        // denied despite the tool being present.
        let mut integration_capabilities = Vec::new();
        let engine_registry = integration_tooling
            .into_registry_recording(&mut integration_capabilities, grant_tooling.registry);

        let subject = dirs.local_subject();
        let holder = GateHolderId(subject.clone());

        // Mint the per-session cap-token, anchored at process start.
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut tool_allowlist = vec![
            TOOL.to_string(),
            ardur_memory::MEMORY_READ_CAPABILITY.to_string(),
            ardur_memory::MEMORY_WRITE_CAPABILITY.to_string(),
            SESSION_CHECKPOINT_CAPABILITY.to_string(),
            SESSION_ROLLBACK_CAPABILITY.to_string(),
            CONTEXT_COMPACT_CAPABILITY.to_string(),
            BACKGROUND_TASK_CAPABILITY.to_string(),
            STEER_CAPABILITY.to_string(),
            INTERRUPT_CAPABILITY.to_string(),
        ];
        tool_allowlist.extend(grant_tooling.extra_allowlist.iter().cloned());
        tool_allowlist.extend(integration_capabilities.iter().cloned());
        let cap = issuer
            .issue(
                CapHolderId(subject.clone()),
                CapScope {
                    audience: AUDIENCE.to_string(),
                    expires_unix: now_unix + CAP_TTL_SECS,
                    // The verifier checks the per-turn cost (1 unit) against this
                    // ceiling, so it must be at least 1.
                    budget_remaining: budget_cents.max(1),
                    tool_allowlist,
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
        let journal = Arc::new(
            FileSessionJournal::new(&dirs.journals, session_id)
                .map_err(|e| CliError::State(format!("opening the session journal: {e}")))?,
        );
        // TODO §7.0: no file-backed `MemoryRuntime` exists yet, so the bi-temporal
        // memory sink is in-process for now (the `~/.ardur/memory/` dir is created
        // for the persistent store that replaces this).
        let memory = Arc::new(InMemoryMemoryRuntime::new());

        // #502 Seam B7 follow-up: the opt-in governance ER mirror
        // (`ARDUR_GOVERNANCE`, default off). Opened under the state root at the
        // DESIGN.md convention `<root>/governance/er-chain.jsonl`, from the SAME
        // P-256 custody key native receipts sign with (both live under `keys/`).
        // The open is fail-closed: a corrupt or foreign-key mirror log fails the
        // session start rather than re-genesis chains. Default off constructs
        // nothing — byte-identical to a session without the seam.
        let governance = (governance_mirror_enabled())
            .then(|| {
                ErMirrorEmitter::open_in_data_dir(&dirs.root, &receipt_key, GOVERNANCE_VERIFIER_ID)
                    .map_err(|e| {
                        CliError::State(format!(
                            "opening the governance ER mirror in {}: {e} \
                             (unset ARDUR_GOVERNANCE or repair {})",
                            dirs.root.display(),
                            dirs.root.join("governance").display()
                        ))
                    })
            })
            .transpose()?;
        if let Some(emitter) = &governance {
            tracing::info!(
                mirror = %emitter.path().display(),
                "governance ER mirror enabled"
            );
        }

        let (runtime, reconciliation) = FusedRuntimeBuilder::new(
            cap_root,
            policies.clone(),
            provider,
            receipt_key,
            model.clone(),
        )
        .require_durable_settlements()
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
        .with_journal(journal.clone())
        // ARD-457: the operator-granted hardened tools (empty registry when no
        // grants exist — fail-closed).
        // ARD-459: integration tools are folded into the same registry, so a
        // configured integration is actually callable rather than merely
        // reported healthy by doctor.
        .with_tools(Arc::new(engine_registry))
        // ARD-H1: install the built-in injection-defense signatures so `ardur
        // chat` scans prompts too, rather than shipping stage 4.5 inert.
        .with_default_injection_filters()
        .receipt_log(dirs.receipt_log())
        // #502 Seam B7 follow-up: hand the opt-in ER mirror over. `None` (the
        // default) keeps the session byte-identical to one built before the
        // seam; the runtime fires the emitter at the commit decision only, so
        // abandoned/cancelled turns mint no ER.
        .maybe_with_governance(
            governance.map(|emitter| Arc::new(emitter) as Arc<dyn GovernanceEmitter>),
        )
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
            settlements: SettlementLifecycle::new(&runtime, journal),
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

    /// Inspect/retry retained settlement work, including after an externally
    /// cancelled `stream_turn` future. Keep this handle until safe closure.
    pub fn settlement_supervisor(&self) -> ardur_fused_runtime::settlement::SettlementSupervisor {
        self.settlements.supervisor.clone()
    }

    /// Explicit normal-exit drain and journal close. Failure leaves this engine
    /// owning unresolved work; process destruction is not a successful shutdown.
    pub async fn shutdown(&self) -> Result<(), CliError> {
        self.settlements.finish().await
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
    /// journal entry, but — gh#533 — the provider send is admitted (Cedar +
    /// cost gate) and audited with a `context.compact.previewed.v1` receipt.
    ///
    /// # Errors
    /// Returns [`CliError`] if the session cap-token does not grant the
    /// compact capability, Cedar denies the action, the budget cannot cover
    /// the call, or the provider call fails.
    pub async fn preview_compact(
        &self,
        history: &[ChatMessage],
        focus: Option<String>,
    ) -> Result<String, CliError> {
        self.runtime
            .preview_compact(
                self.session_id,
                &self.cap_token,
                CONTEXT_COMPACT_CAPABILITY,
                history,
                focus,
            )
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
    /// force. Drop records cancellation synchronously; this owner drains its
    /// journal projection after the owning stream (not only its pin) is dropped.
    pub async fn stream_turn<W: std::io::Write>(
        &self,
        messages: &[ChatMessage],
        out: &mut W,
        ctx: &crate::stream::RenderCtx<'_>,
    ) -> std::io::Result<StreamOutcome> {
        self.consume_stream(messages, async |stream| {
            drive_fused_turn(stream, out, ctx).await
        })
        .await
    }

    /// One owner for both terminal consumers. Only a pinned borrow escapes to
    /// the consumer: the actual source is dropped before settlement is drained.
    /// Callers must retain `settlements` outside any cancellable outer future.
    pub(crate) async fn consume_stream<T>(
        &self,
        messages: &[ChatMessage],
        consume: impl for<'s> AsyncFnOnce(FusedSource<'s>) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        self.settlements
            .drain()
            .await
            .map_err(std::io::Error::other)?;
        let outcome = {
            let source = self.runtime.stream(SubmitRequest {
                messages: messages.to_vec(),
                cap_token: self.cap_token.clone(),
                session_id: self.session_id,
                requested_provider: None,
            });
            futures::pin_mut!(source);
            consume(source).await
        }; // the owning source, not just the pin, is gone here
        let result = self.settlements.after(outcome, std::io::Error::other).await;
        if let Some(balance) = self.runtime.remaining_budget(&self.holder).await {
            self.remaining.store(balance.cents, Ordering::SeqCst);
        }
        result
    }

    /// Receipt notifications can lag durable commit. Rebuild from the journal
    /// after the owning stream has dropped and drained, including on cancel or
    /// output failure. Never promote display-only deltas into the next request.
    pub(crate) async fn reconcile_history(
        &self,
        history: &mut Vec<ChatMessage>,
    ) -> Result<(), CliError> {
        self.replayed_entries()
            .await
            .map(|entries| *history = crate::journal_entries_to_history(&entries))
    }

    /// The drained, replayed journal entries for this session — the durable
    /// ground truth [`reconcile_history`](Self::reconcile_history) rebuilds
    /// history from, exposed so the REPL can also derive its committed-cost
    /// tally from the same read (gh#539: displayed committed cost must equal
    /// the durable ledger even when the Receipt notification was never
    /// delivered).
    pub(crate) async fn replayed_entries(
        &self,
    ) -> Result<Vec<ardur_session_journals::JournalEntry>, CliError> {
        self.settlements.drain().await?;
        self.settlements
            .journal
            .replay(self.session_id)
            .await
            .map_err(|_| CliError::State("could not reconcile durable chat history".into()))
    }

    /// Run one chat turn over `messages` through the full fused pipeline, then
    /// refresh the displayed budget from the cost gate's ledger.
    pub async fn run_turn(&self, messages: &[ChatMessage]) -> Result<TurnOutcome, CliError> {
        self.settlements.drain().await?;
        let result = self
            .runtime
            .submit(SubmitRequest {
                messages: messages.to_vec(),
                cap_token: self.cap_token.clone(),
                session_id: self.session_id,
                requested_provider: None,
            })
            .await
            .map_err(CliError::from);
        // Refresh even on failure, from actual ledger economics (not receipt presence).
        if let Some(balance) = self.runtime.remaining_budget(&self.holder).await {
            self.remaining.store(balance.cents, Ordering::SeqCst);
        }
        let result = self.settlements.after(result, |e| e).await?;

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

/// Tools contributed by configured integrations (ARD-459).
///
/// Registering an adapter's tools in the doctor registry tells an operator the
/// configuration is sound; it does not make the tools callable. This is what
/// puts them in the registry the engine runs with, and their capability labels
/// into the session cap-token.
struct IntegrationTooling {
    /// Built tools, not yet registered — `ToolRegistry` cannot be drained, so
    /// registration is deferred until the grant registry is available to merge
    /// into.
    tools: Vec<Arc<dyn ardur_tool_registry::Tool>>,
    /// Capability labels to mint into the session cap-token. Populated in
    /// `into_registry`, so a capability is granted only if its tool actually
    /// registered — the same invariant `GrantTooling` keeps.
    extra_allowlist: Vec<String>,
}

impl IntegrationTooling {
    /// Build the tools for every enabled integration in `~/.ardur/config.toml`.
    ///
    /// Fail-soft by design: an unreadable or malformed config yields no tools
    /// rather than refusing to start a chat. The server's boot path is the
    /// fail-closed surface for integrations; `ardur chat` should still run when
    /// an unrelated part of the config file is wrong, and `ardur doctor`
    /// reports the problem in detail.
    fn from_config(dirs: &StateDirs) -> Self {
        let path = dirs.root.join("config.toml");
        let Ok(document) = std::fs::read_to_string(&path) else {
            return Self::empty();
        };
        let Ok(mut set) = ardur_integrations::parse_integrations(&document) else {
            tracing::warn!(
                "integration configuration could not be parsed; no integration tools registered \
                 (run `ardur doctor` for the reason)"
            );
            return Self::empty();
        };
        let env: std::collections::BTreeMap<String, String> = std::env::vars()
            .filter(|(k, _)| k.starts_with("ARDUR_INTEGRATIONS_"))
            .collect();
        if ardur_integrations::apply_env_overrides(&mut set, &env).is_err() {
            tracing::warn!(
                "an ARDUR_INTEGRATIONS_* override was rejected; no integration tools registered"
            );
            return Self::empty();
        }

        match integration_registry().build_active(&set) {
            Ok(tools) => Self {
                tools,
                extra_allowlist: Vec::new(),
            },
            Err(e) => {
                // An enabled integration with no adapter, or an adapter that
                // refused its endpoint. Warn and register nothing rather than
                // registering a partial set: half an integration is harder to
                // reason about than none.
                tracing::warn!(error = %e, "integration tools unavailable");
                Self::empty()
            }
        }
    }

    fn empty() -> Self {
        Self {
            tools: Vec::new(),
            extra_allowlist: Vec::new(),
        }
    }

    /// Fold the integration tools into `registry`, reporting the capabilities
    /// that registration actually granted.
    ///
    /// The capabilities come back through `granted_out` rather than a field so
    /// the caller cannot read them before this has run: registration is what
    /// decides which are legitimate, and an allowlist read too early is empty,
    /// which denies every integration call while the tools look present.
    fn into_registry_recording(
        mut self,
        granted_out: &mut Vec<String>,
        mut registry: ToolRegistry,
    ) -> ToolRegistry {
        let mut granted = Vec::new();
        for tool in self.tools.drain(..) {
            let id = tool.id();
            let caps: Vec<String> = tool
                .required_capabilities()
                .iter()
                .map(ardur_tool_registry::Capability::as_str)
                .collect();
            match registry.register(Box::new(ArcTool(tool))) {
                Ok(()) => {
                    tracing::info!(tool = %id, "registered integration tool");
                    granted.push(id.to_string());
                    granted.extend(caps);
                }
                Err(e) => {
                    tracing::warn!(tool = %id, error = %e, "integration tool registration failed");
                }
            }
        }
        granted.sort();
        granted.dedup();
        granted_out.clone_from(&granted);
        self.extra_allowlist = granted;
        registry
    }
}

/// The adapters this binary carries.
///
/// One constructor, used by both the chat runtime and `ardur doctor`, so the
/// two cannot drift into disagreeing about which integrations are supported.
pub fn integration_registry() -> ardur_integrations::AdapterRegistry {
    ardur_integrations::AdapterRegistry::new()
        .with(Arc::new(ardur_integration_beads::BeadsAdapter::new()))
        .with(Arc::new(ardur_integration_obsidian::ObsidianAdapter::new()))
        .with(Arc::new(ardur_integration_dolthub::DolthubAdapter::new()))
}

/// Adapts an `Arc<dyn Tool>` into the `Box<dyn Tool>` the registry stores.
///
/// Adapters hand out `Arc` so a tool can be shared; the registry owns a `Box`.
struct ArcTool(Arc<dyn ardur_tool_registry::Tool>);

#[async_trait::async_trait]
impl ardur_tool_registry::Tool for ArcTool {
    fn id(&self) -> ToolId {
        self.0.id()
    }

    fn schema(&self) -> &ardur_tool_registry::ToolSchema {
        self.0.schema()
    }

    async fn invoke(
        &self,
        ctx: &ardur_tool_registry::ToolContext,
        args: serde_json::Value,
    ) -> Result<ardur_tool_registry::ToolOutput, ardur_tool_registry::ToolError> {
        self.0.invoke(ctx, args).await
    }

    fn required_capabilities(&self) -> &[ardur_tool_registry::Capability] {
        self.0.required_capabilities()
    }
}

#[cfg(test)]
mod grant_tooling_tests {
    use super::*;

    #[tokio::test]
    async fn tui_cancel_between_durable_commit_and_receipt_keeps_history() {
        use futures::StreamExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let dirs = StateDirs {
            root: root.clone(),
            memory: root.join("memory"),
            journals: root.join("journals"),
            receipts: root.join("receipts"),
            keys: root.join("keys"),
        };
        dirs.create().unwrap();
        dirs.write_starter_cedar_policy_if_absent().unwrap();
        let engine = FusedEngine::new(&Config::default(), &dirs, 100)
            .await
            .unwrap();
        let mut history = vec![ChatMessage::user("durable local turn")];
        let outcome = engine
            .consume_stream(&history, async |stream| {
                let mut updates = crate::UpdateStream::new(stream);
                while let Some(update) = updates.next().await {
                    if matches!(
                        update,
                        crate::Update::StageEnd {
                            stage: ardur_fused_runtime::StageKind::CostGateFinalize,
                            ok: true
                        }
                    ) {
                        let outcome = updates.into_outcome();
                        assert!(outcome.receipt_ids.is_empty(), "stop before notification");
                        assert!(!outcome.content.is_empty(), "provider really ran");
                        return Ok(outcome);
                    }
                }
                panic!("did not reach the committed, pre-notification boundary");
            })
            .await
            .unwrap();
        assert!(outcome.receipt_ids.is_empty());
        engine.reconcile_history(&mut history).await.unwrap();
        assert_eq!(
            history.len(),
            2,
            "must read durable history, not receipt visibility"
        );
        assert_eq!(history[1].content, outcome.content);
        let status = engine.settlement_supervisor().status();
        assert!(
            status.turns.is_empty() && status.busy.is_none() && status.executing.is_none(),
            "owning stream must drop before drain: {status:?}"
        );
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn broken_output_drains_owning_stream_before_returning_original_error() {
        use ardur_session_journals::SessionJournal;
        struct BrokenOutput;
        impl std::io::Write for BrokenOutput {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "original stdout failure",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let dirs = StateDirs {
            root: root.clone(),
            memory: root.join("memory"),
            journals: root.join("journals"),
            receipts: root.join("receipts"),
            keys: root.join("keys"),
        };
        dirs.create().unwrap();
        dirs.write_starter_cedar_policy_if_absent().unwrap();
        let engine = FusedEngine::new(&Config::default(), &dirs, 100)
            .await
            .unwrap();
        let theme = crate::theme::Theme::default().plain();
        let error = engine
            .stream_turn(
                &[ChatMessage::user("local")],
                &mut BrokenOutput,
                &crate::stream::RenderCtx::new(&theme, 80),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "original stdout failure");
        let status = engine.runtime.settlement_supervisor().status();
        assert!(
            status.turns.is_empty(),
            "stdout error must not strand cancellation projection: {status:?}"
        );
        let journal = FileSessionJournal::new(&dirs.journals, engine.session_id).unwrap();
        let entries = journal.replay(engine.session_id).await.unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|e| matches!(
                    e,
                    ardur_session_journals::JournalEntry::CostFinalized { .. }
                ))
                .count(),
            1
        );
        assert!(!entries.iter().any(|e| matches!(
            e,
            ardur_session_journals::JournalEntry::AssistantMessage { .. }
        )));
        assert_eq!(engine.remaining_cents(), 100);
        assert!(crate::journal_entries_to_history(&entries).is_empty());

        struct UnknownJournal {
            session: SessionId,
            closed: AtomicU64,
        }
        #[async_trait::async_trait]
        impl SessionJournal for UnknownJournal {
            async fn append(
                &self,
                _: ardur_session_journals::JournalEntry,
            ) -> Result<ardur_session_journals::EntryId, ardur_session_journals::JournalError>
            {
                Err(ardur_session_journals::JournalError::Io(
                    std::io::Error::other("unknown append"),
                ))
            }
            async fn replay(
                &self,
                _: SessionId,
            ) -> Result<
                Vec<ardur_session_journals::JournalEntry>,
                ardur_session_journals::JournalError,
            > {
                Ok(vec![])
            }
            async fn replay_from(
                &self,
                id: SessionId,
                _: ardur_session_journals::EntryId,
            ) -> Result<
                Vec<ardur_session_journals::JournalEntry>,
                ardur_session_journals::JournalError,
            > {
                self.replay(id).await
            }
            async fn close(&self) -> Result<(), ardur_session_journals::JournalError> {
                self.closed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn session_id(&self) -> &SessionId {
                &self.session
            }
        }
        let mut engine = engine;
        let journal = engine.settlements.journal.clone();
        let unknown = Arc::new(UnknownJournal {
            session: engine.session_id,
            closed: AtomicU64::new(0),
        });
        engine.settlements.journal = unknown.clone();
        let error = engine
            .stream_turn(
                &[ChatMessage::user("local again")],
                &mut BrokenOutput,
                &crate::stream::RenderCtx::new(&theme, 80),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "original stdout failure",
            "drain failure must not erase original outcome"
        );
        assert!(
            engine.shutdown().await.is_err(),
            "no clean shutdown with unknown accounting"
        );
        assert_eq!(
            unknown.closed.load(Ordering::SeqCst),
            0,
            "do not close unresolved journal"
        );
        let supervisor = engine.settlement_supervisor();
        assert!(!supervisor.status().turns.is_empty());
        assert!(!supervisor.status().turns[0].pending_projections.is_empty());
        drop(engine); // Retained supervisor outlives the runtime on failure.
        assert!(
            supervisor.drain_pending(journal.as_ref()).await.is_err(),
            "healthy backend alone cannot erase ambiguity"
        );
        assert!(supervisor.try_close().is_err());
    }

    fn record(tool: &str, caps: &[&str], scope: Option<&str>) -> GrantRecord {
        GrantRecord {
            tool: tool.to_string(),
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
            scope: scope.map(str::to_string),
            subject: "cli://localhost-test".to_string(),
            granted_at_ms: 1,
            receipt_id: Some(uuid::Uuid::new_v4().to_string()),
        }
    }

    #[test]
    fn empty_ledger_registers_nothing_and_adds_no_caps() {
        let tooling = GrantTooling::from_records(&[]);
        assert!(tooling.registry.list().is_empty());
        assert!(tooling.extra_allowlist.is_empty());
    }

    #[test]
    fn shell_grant_with_scope_registers_and_mints_caps() {
        let records = [record(
            "shell.run",
            &["cap.shell_exec", "cap.process_spawn"],
            Some("git|cargo"),
        )];
        let tooling = GrantTooling::from_records(&records);
        assert!(tooling.registry.get(&ToolId::new("shell.run")).is_some());
        for cap in ["shell.run", "cap.shell_exec", "cap.process_spawn"] {
            assert!(
                tooling.extra_allowlist.iter().any(|c| c == cap),
                "`{cap}` must join the session allowlist"
            );
        }
    }

    #[test]
    fn shell_grant_without_scope_never_registers_the_unrestricted_shell() {
        let records = [record(
            "shell.run",
            &["cap.shell_exec", "cap.process_spawn"],
            None,
        )];
        let tooling = GrantTooling::from_records(&records);
        assert!(tooling.registry.get(&ToolId::new("shell.run")).is_none());
        assert!(tooling.extra_allowlist.is_empty());
    }

    #[test]
    fn file_grants_register_only_with_a_root_scope() {
        let records = [
            record("file.read", &["cap.fs_read"], Some("/tmp/ardur-files")),
            record("file.write", &["cap.fs_write"], None), // scope-less: skipped
        ];
        let tooling = GrantTooling::from_records(&records);
        assert!(tooling.registry.get(&ToolId::new("file.read")).is_some());
        assert!(tooling.extra_allowlist.iter().any(|c| c == "cap.fs_read"));
        assert!(
            !tooling.extra_allowlist.iter().any(|c| c == "cap.fs_write"),
            "a scope-less file.write grant must not mint cap.fs_write"
        );
    }

    #[test]
    fn http_grant_without_scope_registers_localhost_only_and_mints_network_cap() {
        let records = [record("http.fetch", &["cap.network_out"], None)];
        let tooling = GrantTooling::from_records(&records);
        assert!(tooling.registry.get(&ToolId::new("http.fetch")).is_some());
        assert!(
            tooling
                .extra_allowlist
                .iter()
                .any(|c| c == "cap.network_out")
        );
    }

    #[tokio::test]
    async fn multiple_shell_grants_union_their_scopes() {
        let records = [
            record(
                "shell.run",
                &["cap.shell_exec", "cap.process_spawn"],
                Some("git"),
            ),
            record(
                "shell.run",
                &["cap.shell_exec", "cap.process_spawn"],
                Some("cargo"),
            ),
        ];
        let tooling = GrantTooling::from_records(&records);
        let tool = tooling
            .registry
            .get(&ToolId::new("shell.run"))
            .expect("registered");
        // Behavior-level assertion: BOTH grants' commands run, a third does not.
        use ardur_tool_registry::{InvocationId, ToolContext};
        use std::collections::HashMap;
        let ctx = ToolContext {
            cap_token: CapTokenRef(String::new()),
            session_id: SessionId::new(),
            invocation_id: InvocationId::new(),
            cwd: std::path::PathBuf::from("."),
            env: HashMap::new(),
            cost_budget_cents: u32::MAX,
        };
        let git = tool
            .invoke(&ctx, serde_json::json!({ "command": "git --version" }))
            .await;
        assert!(git.is_ok(), "first grant's scope must run: {git:?}");
        let cargo = tool
            .invoke(&ctx, serde_json::json!({ "command": "cargo --version" }))
            .await;
        assert!(cargo.is_ok(), "second grant's scope must run: {cargo:?}");
        let denied = tool
            .invoke(&ctx, serde_json::json!({ "command": "uname -a" }))
            .await;
        assert!(denied.is_err(), "ungranted commands stay denied");
    }

    #[test]
    fn granting_file_read_does_not_expose_file_write() {
        let records = [record(
            "file.read",
            &["cap.fs_read"],
            Some("/tmp/ardur-files"),
        )];
        let tooling = GrantTooling::from_records(&records);
        assert!(tooling.registry.get(&ToolId::new("file.read")).is_some());
        assert!(
            tooling.registry.get(&ToolId::new("file.write")).is_none(),
            "file.write must not register from a file.read grant"
        );
        assert!(
            tooling.registry.get(&ToolId::new("file.list")).is_none(),
            "file.list must not register from a file.read grant"
        );
        assert!(tooling.extra_allowlist.iter().any(|c| c == "cap.fs_read"));
        assert!(!tooling.extra_allowlist.iter().any(|c| c == "cap.fs_write"));
    }

    #[test]
    fn validation_rejects_wrong_subject_missing_receipt_and_digest_mismatch() {
        use ardur_receipt::{
            HolderId, ReceiptBody, Sha256Digest, TokenId, UnixTsMillis, VerbObject,
        };
        let good = record(
            "shell.run",
            &["cap.shell_exec", "cap.process_spawn"],
            Some("echo"),
        );
        let chain_entry =
            |id: &str, body_for: Option<&GrantRecord>| ardur_fused_runtime::PersistedReceipt {
                jws_compact: format!("h.{id}.s"),
                body: ReceiptBody {
                    receipt_id: uuid::Uuid::parse_str(id).expect("uuid"),
                    parent_hash: None,
                    verb: VerbObject::new("tool.grant.allow.v1").expect("verb"),
                    issued_at: UnixTsMillis(1),
                    subject: HolderId("cli://localhost-test".to_string()),
                    cap_token_id: TokenId(uuid::Uuid::from_bytes(*b"ardur-op-grant!!")),
                    payload_digest: body_for
                        .map(|r| Sha256Digest::of(&GrantTooling::canonical_payload(r)))
                        .unwrap_or_else(|| Sha256Digest::of(b"other")),
                    session_id: None,
                    cost: ardur_receipt::CostTuple {
                        tokens_in: 0,
                        tokens_out: 0,
                        cents: 0,
                        wall_ms: 0,
                        attention_score: 0,
                    },
                    tool_calls: Vec::new(),
                    provider: None,
                },
            };
        let good_id = good.receipt_id.clone().expect("receipt id");
        let chain = vec![chain_entry(&good_id, Some(&good))];

        // 1. Valid record passes.
        let ok = GrantTooling::validate_records(
            std::slice::from_ref(&good),
            "cli://localhost-test",
            &chain,
        );
        assert_eq!(ok.len(), 1);

        // 2. Wrong subject is rejected.
        let mut wrong_subject = good.clone();
        wrong_subject.subject = "cli://localhost-other".to_string();
        assert!(
            GrantTooling::validate_records(&[wrong_subject], "cli://localhost-test", &chain)
                .is_empty()
        );

        // 3. Missing receipt id is rejected.
        let mut no_receipt = good.clone();
        no_receipt.receipt_id = None;
        assert!(
            GrantTooling::validate_records(&[no_receipt], "cli://localhost-test", &chain)
                .is_empty()
        );

        // 4. Receipt exists but commits to a different payload (edited scope) is rejected.
        let mut edited = good.clone();
        edited.scope = Some("rm -rf".to_string());
        assert!(
            GrantTooling::validate_records(&[edited], "cli://localhost-test", &chain).is_empty()
        );

        // 5. A TURN receipt carrying the matching id+digest but a non-grant verb
        //    is not authorization.
        let turn_chain = vec![ardur_fused_runtime::PersistedReceipt {
            jws_compact: format!("h.{good_id}.s"),
            body: ReceiptBody {
                verb: VerbObject::new("llm.completion.minted.v1").expect("verb"),
                ..chain[0].body.clone()
            },
        }];
        assert!(
            GrantTooling::validate_records(
                std::slice::from_ref(&good),
                "cli://localhost-test",
                &turn_chain
            )
            .is_empty()
        );
    }

    #[test]
    fn a_scopeless_http_grant_preserves_localhost_when_scoped_grants_join() {
        let records = [
            record("http.fetch", &["cap.network_out"], Some("example.com")),
            record("http.fetch", &["cap.network_out"], None),
        ];
        let tooling = GrantTooling::from_records(&records);
        assert!(tooling.registry.get(&ToolId::new("http.fetch")).is_some());
        // The scoped hosts union; localhost intent rides the tool's
        // allow_localhost knob (covers the FULL loopback range, not literals —
        // tool-registry tests assert 127.0.0.2/::1 admission).
        assert!(tooling.http_hosts.iter().any(|h| h == "example.com"));
        assert!(tooling.http_localhost);

        // A scoped grant alone must NOT gain localhost.
        let scoped_only = GrantTooling::from_records(&[record(
            "http.fetch",
            &["cap.network_out"],
            Some("example.com"),
        )]);
        assert!(!scoped_only.http_localhost);
    }

    #[test]
    fn unknown_ledger_tools_are_skipped() {
        let records = [record("nuke.everything", &["cap.all"], Some("*"))];
        let tooling = GrantTooling::from_records(&records);
        assert!(tooling.registry.list().is_empty());
        assert!(tooling.extra_allowlist.is_empty());
    }
}

#[cfg(test)]
mod integration_tooling_tests {
    use super::*;

    /// A configured integration's tools must reach the registry the engine
    /// runs with, and their capabilities the session cap-token.
    ///
    /// Registering adapters in the doctor registry only reports the
    /// configuration as sound. Before this, `ardur doctor` said an integration
    /// was healthy while chat could not invoke a single one of its tools —
    /// the same reachability mistake as the adapter never being wired in at
    /// all, one layer further in.
    #[test]
    fn configured_integration_tools_join_the_engine_registry_and_allowlist() {
        let vault = tempfile::tempdir().expect("tempdir");
        let set = ardur_integrations::parse_integrations(&format!(
            "[integrations.obsidian]\nroot = \"{}\"\nenabled = true\n",
            vault.path().display()
        ))
        .expect("fixture parses");

        let tools = integration_registry()
            .build_active(&set)
            .expect("the obsidian adapter builds");
        let tooling = IntegrationTooling {
            tools,
            extra_allowlist: Vec::new(),
        };

        // Fold into an empty grant registry, as a no-grants install would.
        let mut registry = ToolRegistry::new();
        let mut allowlist = Vec::new();
        let folded = {
            let t = tooling;
            let r = t.into_registry_recording(&mut allowlist, registry);
            registry = ToolRegistry::new();
            let _ = &registry;
            r
        };

        assert!(
            folded.get(&ToolId::new("obsidian.read")).is_some(),
            "obsidian.read must be callable from the engine registry"
        );
        assert!(folded.get(&ToolId::new("obsidian.write")).is_some());

        // And its capabilities must be mintable into the session cap-token,
        // or every call is denied despite the tool existing.
        assert!(
            allowlist.contains(&"cap.integration.obsidian.read".to_string()),
            "the read capability must join the allowlist: {allowlist:?}"
        );
        assert!(
            allowlist.contains(&"cap.fs_read".to_string()),
            "the nested filesystem capability must join too: {allowlist:?}"
        );
    }

    /// No configuration means no integration tools — the default posture.
    #[test]
    fn an_install_with_no_integrations_registers_nothing() {
        let tooling = IntegrationTooling::empty();
        let mut allowlist = Vec::new();
        let folded = tooling.into_registry_recording(&mut allowlist, ToolRegistry::new());

        assert!(
            folded.list().is_empty(),
            "a fresh install registers no integration tools"
        );
        assert!(
            allowlist.is_empty(),
            "and mints no integration capabilities"
        );
    }
}
