//! The in-process model router (#411 / #531 D0): task-class lanes, ordered
//! failover, credential-pool rotation, and operator `model_overrides` —
//! additive to the existing provider crates, and honest on receipts.
//!
//! # Model
//!
//! A *lane* is a named, ordered chain of [`ChainEntry`] values. The entry a
//! request runs against is chosen by its task class — the request's `model`
//! field, which is the one per-call signal the sealed [`Provider`] trait
//! carries. A request whose model matches no lane falls back to the
//! configured default lane (logged once per unknown class, not per call).
//!
//! [`RouterProvider::complete`] / [`RouterProvider::stream`] try the lane's
//! entries in order and advance **only** on typed-retryable failures
//! ([`classify`]): rate-limits, transport failures, funneled upstream 5xx,
//! and auth failures when a *different* credential follows in the chain.
//! Everything else — invalid requests, unavailable models, cost ceilings,
//! selection errors, non-5xx upstream failures — surfaces immediately, because
//! retrying it against another backend would bill the same rejection again.
//!
//! # Receipt honesty
//!
//! The runtime mints receipts with `provider: Some(provider.name())`, and
//! `name()` here is *dynamic*: it reports the backend that actually served the
//! most recent call, plus the failover path when one was taken:
//!
//! - served on the first attempt: `"anthropic"` — byte-identical to the
//!   receipt an unroutered provider produces;
//! - served after failover/rotation: `"anthropic->anthropic#1->openrouter"`
//!   (each element is the backend, with `#n` marking a pool credential beyond
//!   the first on the same backend);
//! - all entries failed: `"router:failed(anthropic->openrouter)"`.
//!
//! The runtime reads `name()` at receipt-mint time inside the same sequential
//! turn flow the call ran in, so the mark reflects *this* turn's call. Before
//! any call, `name()` is the static `"router"`.
//!
//! # Credential pools
//!
//! The selector expands one configured lane entry into several [`ChainEntry`]s
//! — one per pooled key — each carrying a non-secret [`CredentialId`]
//! (`backend` + pool index). Key material never reaches this module, so it
//! cannot leak into logs, errors, receipts, or `Debug` output here. A
//! rate-limited credential cools for the provider's `retry_after_ms` (or
//! [`RATE_LIMIT_FALLBACK_COOLDOWN`] when the provider says "0"); an
//! unauthorized credential is benched for
//! [`UNAUTHORIZED_CREDENTIAL_COOLDOWN`]. Cooldowns run on a monotonic clock
//! ([`Instant`]), immune to wall-clock jumps.
//!
//! # `model_overrides`
//!
//! Each entry's effective rate card is its provider's published card patched
//! by the operator's [`ModelOverrideSpec`] (`input/output_price_micros`, in
//! millionths of a US cent per 1k tokens). A field left absent keeps the
//! provider's value — the table never invents prices. `rate_card()` reports
//! the effective card of the entry that served the most recent call (the
//! exact value the runtime's `observe_usage` prices receipts with), so a
//! cost-gate projection reflects the override. An override key matching no
//! entry's model is a loud construction-time warning, never silent.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ardur_runtime::ProviderId;
use async_trait::async_trait;

use crate::error::ProviderError;
use crate::provider::Provider;
use crate::rate_card::RateCard;
use crate::stream::ProviderStream;
use crate::types::{CompletionRequest, CompletionResponse, ModelId};

/// Cooldown applied to a rate-limited credential whose error carried no
/// positive `retry_after_ms`. Long enough that the next turn does not hot-loop
/// the same key, short enough that a transient limit clears on its own.
pub const RATE_LIMIT_FALLBACK_COOLDOWN: Duration = Duration::from_secs(60);

/// How long a credential the provider rejected (`401`) is benched. The key is
/// dead, not hot — only operator intervention (or a process restart, which
/// clears the table) should bring it back quickly.
pub const UNAUTHORIZED_CREDENTIAL_COOLDOWN: Duration = Duration::from_secs(3600);

/// What the router does after a failed attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failover {
    /// Try the next chain entry.
    Advance,
    /// Return the error to the caller now.
    Surface,
}

/// Classify a provider failure for failover.
///
/// The match is deliberately **exhaustive with no wildcard arm**: a future
/// [`ProviderError`] variant breaks this build and forces an explicit
/// retryable/not decision, rather than silently inheriting a default.
///
/// `Upstream` carries no typed status, so the sub-classifier reads the
/// providers' funnel format (`"HTTP {code}: …"`, emitted identically by the
/// anthropic, openrouter, openai-compat, and ollama backends) and treats only
/// 5xx as retryable. Anything it cannot establish — ambiguous text, 4xx,
/// provider-specific messages — is **not** retryable (fail-closed).
///
/// `Unauthorized` advances only when `has_next_credential` is true: the same
/// request against a *different* credential may succeed, while re-sending the
/// dead key anywhere is pointless.
fn classify(err: &ProviderError, has_next_credential: bool) -> Failover {
    match err {
        ProviderError::RateLimited { .. } => Failover::Advance,
        ProviderError::NetworkFailure(_) => Failover::Advance,
        ProviderError::Upstream(message)
            if funneled_http_status(message).is_some_and(|code| (500..=599).contains(&code)) =>
        {
            Failover::Advance
        }
        ProviderError::Upstream(_) => Failover::Surface,
        ProviderError::Unauthorized if has_next_credential => Failover::Advance,
        ProviderError::Unauthorized => Failover::Surface,
        ProviderError::InvalidRequest(_) => Failover::Surface,
        ProviderError::ModelNotAvailable(_) => Failover::Surface,
        ProviderError::CostCeilingExceeded => Failover::Surface,
        ProviderError::InvalidSelection(_) => Failover::Surface,
        ProviderError::UnknownProvider { .. } => Failover::Surface,
    }
}

/// Parse the status code out of the providers' funneled
/// `Upstream("HTTP {code}: …")` format. `None` for anything else — the
/// caller treats `None` as "not established", i.e. not retryable.
fn funneled_http_status(message: &str) -> Option<u16> {
    let rest = message.strip_prefix("HTTP ")?;
    let (code, _) = rest.split_once(':')?;
    code.trim().parse().ok()
}

/// A non-secret handle for one pool credential: the backend spelling plus the
/// key's index in its pool (`0` is the provider's ambient/single credential).
/// Key material is never stored in this module, so `Debug`/`Display`-adjacent
/// output cannot leak it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CredentialId {
    /// The backend this credential belongs to (e.g. `"anthropic"`).
    pub backend: String,
    /// Index into the backend's credential pool.
    pub index: usize,
}

/// Operator metadata patch for one model — the runtime-side twin of
/// `ardur_config::router::ModelOverride` (defined locally so this crate stays
/// below the config crate in the dependency graph). Prices are micro-cents
/// (millionths of a US cent) per 1,000 tokens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelOverrideSpec {
    /// Total context window in tokens, when the operator states it. No runtime
    /// consumer exists yet; exposed via [`RouterProvider::context_window`].
    pub context_window: Option<u64>,
    /// Price per 1,000 input tokens, in millionths of a US cent.
    pub input_price_micros: Option<u64>,
    /// Price per 1,000 output tokens, in millionths of a US cent.
    pub output_price_micros: Option<u64>,
}

/// One pre-built link in a lane's failover chain.
///
/// Entries are constructed by the selector (which can see the concrete
/// provider crates); this module only routes across them.
pub struct ChainEntry {
    /// The backend spelling this entry was built from.
    backend: String,
    /// The model completions are pinned to on this entry.
    model: ModelId,
    /// The live provider instance (one per pooled credential).
    provider: Arc<dyn Provider>,
    /// Which pool credential this entry uses.
    credential: CredentialId,
    /// The provider's rate card with any model override applied.
    card: RateCard,
}

impl ChainEntry {
    /// Wrap a built provider as a chain entry, computing its effective rate
    /// card from the provider's own card plus `patch` (when given).
    #[must_use]
    pub fn new(
        backend: impl Into<String>,
        model: ModelId,
        provider: Arc<dyn Provider>,
        credential: CredentialId,
        patch: Option<&ModelOverrideSpec>,
    ) -> Self {
        let backend = backend.into();
        let mut card = provider.rate_card().clone();
        if let Some(patch) = patch {
            let mut touched = false;
            if let Some(micros) = patch.input_price_micros {
                card.cents_per_1k_input = micros as f64 / 1_000_000.0;
                touched = true;
            }
            if let Some(micros) = patch.output_price_micros {
                card.cents_per_1k_output = micros as f64 / 1_000_000.0;
                touched = true;
            }
            if touched {
                // The priced provenance hash (`CostProvenance::PricedUsage`)
                // covers the serialized card; a distinct version id keeps an
                // overridden card auditable against the provider's published one.
                card.version_id = format!("{}+override", card.version_id);
            }
        }
        Self {
            backend,
            model,
            provider,
            credential,
            card,
        }
    }
}

/// Why router construction refused a configuration. Every variant names the
/// offending lane so boot logs point at the fix.
#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    /// The configured default lane is absent from the lane map (or the map is
    /// empty) — routing would have nowhere to fall back to.
    #[error("router default lane '{0}' is not defined in [router].lanes")]
    MissingDefaultLane(String),
    /// A lane with zero chain entries can never serve a request.
    #[error("router lane '{lane}' has an empty chain")]
    EmptyLane {
        /// The lane with no entries.
        lane: String,
    },
}

/// The recorded outcome of the most recent call — the data [`Provider::name`]
/// renders honestly.
struct ServedMark {
    /// Every attempted `(backend, credential index)` in order; on success the
    /// serving entry is last.
    attempts: Vec<(String, usize)>,
    /// Whether the last attempt served or the chain exhausted.
    served: bool,
}

/// Lock a mutex, recovering from poisoning: the marks/cooldowns behind these
/// locks are plain data, so a panicked holder leaves them readable, and
/// wedging the provider forever would be the worse failure.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The router itself: one [`Provider`] that dispatches across task-class lanes
/// of pre-built entries.
pub struct RouterProvider {
    /// Flat store of every chain entry; lanes index into it, and
    /// [`Provider::rate_card`] borrows from it through `current`.
    entries: Vec<ChainEntry>,
    /// Task class -> ordered indices into `entries`.
    lanes: HashMap<String, Vec<usize>>,
    /// The fallback lane's entry indices (validated non-empty at construction).
    default_lane: Vec<usize>,
    /// Operator override specs by model id (for `context_window` exposure).
    overrides: HashMap<String, ModelOverrideSpec>,
    /// Override keys that matched no entry's model (warned at construction).
    unknown_override_models: Vec<String>,
    /// Index of the entry that served the most recent call.
    current: AtomicUsize,
    /// The most recent call's honesty mark.
    mark: Mutex<Option<ServedMark>>,
    /// Per-credential cooldown expiries (monotonic clock).
    cooldowns: Mutex<HashMap<CredentialId, Instant>>,
    /// Unknown task classes already warned about (log once, not per call).
    warned_classes: Mutex<HashSet<String>>,
}

impl RouterProvider {
    /// Build a router from ordered `(task_class, chain)` lanes plus the
    /// operator's override table.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::MissingDefaultLane`] when `default` names no
    /// configured lane, and [`RouterError::EmptyLane`] when any lane (default
    /// included) has zero chain entries. Both abort boot — a router that
    /// cannot fall back or cannot serve is a misconfiguration, not a degraded
    /// mode.
    pub fn new(
        default: &str,
        lanes: Vec<(String, Vec<ChainEntry>)>,
        overrides: HashMap<String, ModelOverrideSpec>,
    ) -> Result<Self, RouterError> {
        let mut entries = Vec::new();
        let mut lane_map: HashMap<String, Vec<usize>> = HashMap::new();
        for (class, chain) in lanes {
            if chain.is_empty() {
                return Err(RouterError::EmptyLane { lane: class });
            }
            let indices = lane_map.entry(class).or_default();
            for entry in chain {
                indices.push(entries.len());
                entries.push(entry);
            }
        }
        let default_lane = lane_map
            .get(default)
            .cloned()
            .ok_or_else(|| RouterError::MissingDefaultLane(default.to_string()))?;

        let known_models: HashSet<&str> = entries.iter().map(|e| e.model.0.as_str()).collect();
        let unknown_override_models: Vec<String> = overrides
            .keys()
            .filter(|model| !known_models.contains(model.as_str()))
            .cloned()
            .collect();
        if !unknown_override_models.is_empty() {
            tracing::warn!(
                models = ?unknown_override_models,
                "[router].model_overrides names model id(s) no lane entry uses; \
                 the override(s) have no effect — check for typos"
            );
        }

        let current = default_lane[0];
        Ok(Self {
            entries,
            lanes: lane_map,
            default_lane,
            overrides,
            unknown_override_models,
            current: AtomicUsize::new(current),
            mark: Mutex::new(None),
            cooldowns: Mutex::new(HashMap::new()),
            warned_classes: Mutex::new(HashSet::new()),
        })
    }

    /// The number of task-class lanes configured (pool expansion included in
    /// each lane's chain length). Diagnostics surface.
    #[must_use]
    pub fn lane_count(&self) -> usize {
        self.lanes.len()
    }

    /// The number of chain entries a lane holds (after the selector's
    /// pool expansion). Diagnostics surface for operators and tests.
    #[must_use]
    pub fn lane_len(&self, class: &str) -> Option<usize> {
        self.lanes.get(class).map(Vec::len)
    }

    /// The unknown task classes that have fallen back to the default lane so
    /// far (each warned exactly once). Diagnostics surface.
    #[must_use]
    pub fn fallback_warned_classes(&self) -> Vec<String> {
        let mut classes: Vec<String> = lock(&self.warned_classes).iter().cloned().collect();
        classes.sort();
        classes
    }

    /// Override keys that matched no lane entry's model at construction
    /// (already warned loudly). Diagnostics surface.
    #[must_use]
    pub fn unknown_override_models(&self) -> &[String] {
        &self.unknown_override_models
    }

    /// The operator-stated context window for `model`, if the override table
    /// declares one. No runtime consumer exists yet — this is the seam the
    /// post-D0 routing/context lanes read.
    #[must_use]
    pub fn context_window(&self, model: &ModelId) -> Option<u64> {
        self.overrides
            .get(&model.0)
            .and_then(|spec| spec.context_window)
    }

    /// Resolve the lane a request routes to. The request's `model` field is
    /// the task-class signal; an unrecognized class falls back to the default
    /// lane, warned once per class rather than once per call.
    fn lane_for(&self, class: &str) -> &[usize] {
        if let Some(indices) = self.lanes.get(class) {
            return indices;
        }
        let mut warned = lock(&self.warned_classes);
        if warned.insert(class.to_string()) {
            tracing::warn!(
                task_class = class,
                "[router] request named an unknown task class; routing to the \
                 default lane (this warning appears once per class)"
            );
        }
        &self.default_lane
    }

    /// Is this credential currently benched? Expired entries are evicted
    /// lazily, so a cooled credential becomes eligible the moment its
    /// duration passes.
    fn is_cooling(&self, credential: &CredentialId) -> bool {
        let mut cooldowns = lock(&self.cooldowns);
        match cooldowns.get(credential) {
            Some(until) if *until > Instant::now() => true,
            Some(_) => {
                cooldowns.remove(credential);
                false
            }
            None => false,
        }
    }

    /// Bench a failed credential for the duration its error class dictates.
    fn cool_credential(&self, credential: &CredentialId, err: &ProviderError) {
        let duration = match err {
            ProviderError::RateLimited { retry_after_ms } if *retry_after_ms > 0 => {
                Duration::from_millis(*retry_after_ms)
            }
            ProviderError::RateLimited { .. } => RATE_LIMIT_FALLBACK_COOLDOWN,
            ProviderError::Unauthorized => UNAUTHORIZED_CREDENTIAL_COOLDOWN,
            _ => return,
        };
        lock(&self.cooldowns).insert(credential.clone(), Instant::now() + duration);
    }

    /// Does any entry *after* `position` in the lane use a different
    /// credential than the entry at `position`? That is the
    /// "auth-with-different-credentials-next" condition.
    fn has_next_credential(&self, lane: &[usize], position: usize) -> bool {
        let current = &self.entries[lane[position]].credential;
        lane[position + 1..]
            .iter()
            .any(|idx| self.entries[*idx].credential != *current)
    }

    /// The shortest remaining cooldown across the lane's entries — the honest
    /// `retry_after_ms` when every credential is benched.
    fn min_cooldown_remaining_ms(&self, lane: &[usize]) -> u64 {
        let now = Instant::now();
        let cooldowns = lock(&self.cooldowns);
        lane.iter()
            .filter_map(|idx| cooldowns.get(&self.entries[*idx].credential))
            .filter_map(|until| until.checked_duration_since(now))
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .min()
            .unwrap_or(0)
    }

    /// Record the call's honesty mark and (on success) the serving entry.
    fn record(&self, attempts: Vec<(String, usize)>, served: Option<usize>) {
        if let Some(idx) = served {
            self.current.store(idx, Ordering::Relaxed);
        }
        *lock(&self.mark) = Some(ServedMark {
            attempts,
            served: served.is_some(),
        });
    }

    /// Run one call down the lane's chain. Shared by `complete` and `stream`:
    /// `attempt` performs the entry's call and returns either a value to hand
    /// to the caller or the error to classify.
    async fn dispatch<T>(
        &self,
        class: &str,
        attempt: impl Fn(ChainEntryRef<'_>) -> DispatchFuture<'_, T>,
    ) -> Result<T, ProviderError> {
        let lane = self.lane_for(class);
        let mut attempts: Vec<(String, usize)> = Vec::new();
        let mut last_err: Option<ProviderError> = None;
        let mut saw_cooling = false;

        for (position, idx) in lane.iter().enumerate() {
            let entry = &self.entries[*idx];
            if self.is_cooling(&entry.credential) {
                saw_cooling = true;
                continue;
            }
            attempts.push((entry.backend.clone(), entry.credential.index));
            match attempt(ChainEntryRef { entry }).await {
                Ok(value) => {
                    self.record(attempts, Some(*idx));
                    return Ok(value);
                }
                Err(err) => {
                    let advance = classify(&err, self.has_next_credential(lane, position));
                    match advance {
                        Failover::Advance => {
                            self.cool_credential(&entry.credential, &err);
                            last_err = Some(err);
                        }
                        Failover::Surface => {
                            self.record(attempts, None);
                            return Err(err);
                        }
                    }
                }
            }
        }

        // The chain exhausted. If every remaining entry was benched and we
        // never got a typed failure, the honest surface is a rate-limit with
        // the earliest time a credential comes back.
        let err = match last_err {
            Some(err) => err,
            None => {
                debug_assert!(
                    saw_cooling,
                    "a non-empty lane that dispatched nothing must have been cooling"
                );
                ProviderError::RateLimited {
                    retry_after_ms: self.min_cooldown_remaining_ms(lane),
                }
            }
        };
        self.record(attempts, None);
        Err(err)
    }
}

/// Borrowed view of one entry during a dispatch attempt.
struct ChainEntryRef<'a> {
    entry: &'a ChainEntry,
}

/// The boxed future a dispatch attempt returns.
type DispatchFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, ProviderError>> + Send + 'a>>;

#[async_trait]
impl Provider for RouterProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let class = req.model.0.clone();
        self.dispatch(&class, move |ChainEntryRef { entry }| {
            let mut attempt_req = req.clone();
            attempt_req.model = entry.model.clone();
            Box::pin(async move {
                let mut resp = entry.provider.complete(attempt_req).await?;
                // Re-price with the entry's effective card so the completion's
                // cost matches the card `rate_card()` reports and the receipt
                // path will record. A provider-reported actual billed cost
                // (`usage.cost_cents`) still wins inside `RateCard::price`.
                resp.cost = entry.card.price(resp.usage);
                Ok(resp)
            })
        })
        .await
    }

    async fn stream(&self, req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        let class = req.model.0.clone();
        self.dispatch(&class, move |ChainEntryRef { entry }| {
            let mut attempt_req = req.clone();
            attempt_req.model = entry.model.clone();
            Box::pin(async move { entry.provider.stream(attempt_req).await })
        })
        .await
        // Failover applies to STARTING a stream only. An `Err` item yielded by
        // the returned stream mid-flight passes through untouched: replaying a
        // partially emitted completion on another backend would double-bill
        // and double-emit tokens, so there is deliberately no token-level
        // failover (stated in RUN.md).
    }

    fn id(&self) -> ProviderId {
        ProviderId("router".to_string())
    }

    fn name(&self) -> String {
        let mark = lock(&self.mark);
        match mark.as_ref() {
            None => "router".to_string(),
            Some(mark) if mark.attempts.is_empty() && !mark.served => {
                // Every credential was cooling: nothing was dispatched, so no
                // backend may be named — the honest mark is the state itself.
                "router:cooling".to_string()
            }
            Some(mark) => {
                let path = mark
                    .attempts
                    .iter()
                    .map(|(backend, index)| {
                        if *index == 0 {
                            backend.clone()
                        } else {
                            format!("{backend}#{index}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("->");
                if mark.served {
                    path
                } else {
                    format!("router:failed({path})")
                }
            }
        }
    }

    fn supports_streaming(&self) -> bool {
        // The entry expected to serve: the default lane's first link. Entries
        // without a native stream fall back to the trait's replay impl either
        // way, so this is a delivery-quality hint, not a capability gate.
        self.entries[self.default_lane[0]]
            .provider
            .supports_streaming()
    }

    fn rate_card(&self) -> &RateCard {
        // The effective card of the entry that served the most recent call —
        // the exact value `observe_usage` prices receipts with. Before any
        // call this is the default lane's first entry, which is also the
        // honest basis for a pre-dispatch projection.
        &self.entries[self.current.load(Ordering::Relaxed)].card
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn funneled_status_parses_only_the_funnel_format() {
        assert_eq!(
            funneled_http_status("HTTP 503: service unavailable"),
            Some(503)
        );
        assert_eq!(funneled_http_status("HTTP 500: boom"), Some(500));
        assert_eq!(funneled_http_status("HTTP 599: edge"), Some(599));
        assert_eq!(funneled_http_status("HTTP 400: bad request"), Some(400));
        assert_eq!(
            funneled_http_status("rate limit exceeded for api key"),
            None
        );
        assert_eq!(funneled_http_status("HTTPX 503: nope"), None);
        assert_eq!(funneled_http_status("HTTP : nope"), None);
        assert_eq!(funneled_http_status(""), None);
    }

    #[test]
    fn classify_is_exhaustive_and_fail_closed() {
        // Every ProviderError variant appears here exactly once (12 cases for
        // 9 variants: Unauthorized is pinned under both credential
        // conditions, Upstream under 5xx/4xx/ambiguous). A new variant added
        // to ProviderError breaks the BUILD of `classify` (no wildcard arm),
        // and this table forces the matching test decision to be recorded.
        let cases: Vec<(ProviderError, bool, Failover)> = vec![
            (
                ProviderError::RateLimited { retry_after_ms: 0 },
                false,
                Failover::Advance,
            ),
            (
                ProviderError::NetworkFailure("timeout".to_string()),
                false,
                Failover::Advance,
            ),
            (
                ProviderError::Upstream("HTTP 503: unavailable".to_string()),
                false,
                Failover::Advance,
            ),
            (
                ProviderError::Upstream("HTTP 400: bad request".to_string()),
                false,
                Failover::Surface,
            ),
            (
                ProviderError::Upstream("decoding response body: eof".to_string()),
                false,
                Failover::Surface,
            ),
            (ProviderError::Unauthorized, true, Failover::Advance),
            (ProviderError::Unauthorized, false, Failover::Surface),
            (
                ProviderError::InvalidRequest("bad param".to_string()),
                true,
                Failover::Surface,
            ),
            (
                ProviderError::ModelNotAvailable(ModelId::new("m")),
                true,
                Failover::Surface,
            ),
            (ProviderError::CostCeilingExceeded, true, Failover::Surface),
            (
                ProviderError::InvalidSelection("nope".to_string()),
                true,
                Failover::Surface,
            ),
            (
                ProviderError::UnknownProvider {
                    name: "nope".to_string(),
                    supported: vec![],
                },
                true,
                Failover::Surface,
            ),
        ];
        // Population pin: "every variant classified" is vacuous without a count.
        assert_eq!(cases.len(), 12, "every variant + both auth conditions");
        for (err, has_next, expected) in &cases {
            assert_eq!(
                classify(err, *has_next),
                *expected,
                "classification of {err:?} with has_next_credential={has_next}"
            );
        }
    }
}
