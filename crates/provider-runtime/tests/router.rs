//! Behavioral evidence for the model router (#411 / #531 D0): failover across
//! an ordered chain, credential-pool rotation, model overrides, and receipt
//! honesty — all over stub providers, no network.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use ardur_provider_runtime::{
    ChainEntry, CompletionRequest, CompletionResponse, CredentialId, FinishReason, ModelId,
    ModelOverrideSpec, Provider, ProviderError, RateCard, RouterError, RouterProvider, Usage,
};
use async_trait::async_trait;

/// One scripted outcome for a stub call.
enum StubStep {
    Ok(&'static str),
    Err(ProviderError),
}

/// A provider that plays back a script of outcomes, counting calls so tests
/// can prove which entries were (and were NOT) dispatched.
struct StubProvider {
    id: &'static str,
    script: Mutex<VecDeque<StubStep>>,
    calls: AtomicUsize,
    card: RateCard,
    streaming: bool,
}

impl StubProvider {
    fn new(id: &'static str, script: Vec<StubStep>) -> Arc<Self> {
        Arc::new(Self {
            id,
            script: Mutex::new(script.into()),
            calls: AtomicUsize::new(0),
            card: RateCard {
                version_id: format!("{id}-v1"),
                cents_per_1k_input: 0.3,
                cents_per_1k_output: 1.5,
                cents_per_request: 0.0,
            },
            streaming: false,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for StubProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let step = self
            .script
            .lock()
            .expect("script lock")
            .pop_front()
            .unwrap_or(StubStep::Ok("default-content"));
        match step {
            StubStep::Ok(content) => {
                let usage = Usage {
                    tokens_in: 100,
                    tokens_out: 50,
                    cost_cents: None,
                };
                Ok(CompletionResponse {
                    content: content.to_string(),
                    finish_reason: FinishReason::Stop,
                    usage,
                    cost: self.card.price(usage),
                    raw_provider_response: None,
                })
            }
            StubStep::Err(err) => Err(err),
        }
    }

    fn id(&self) -> ardur_provider_runtime::ProviderId {
        ardur_provider_runtime::ProviderId(self.id.to_string())
    }

    fn supports_streaming(&self) -> bool {
        self.streaming
    }

    fn rate_card(&self) -> &RateCard {
        &self.card
    }
}

/// Build a chain entry for `provider` as pool credential `index` of `backend`.
fn entry(
    backend: &str,
    model: &str,
    provider: Arc<dyn Provider>,
    index: usize,
    patch: Option<&ModelOverrideSpec>,
) -> ChainEntry {
    ChainEntry::new(
        backend,
        ModelId::new(model),
        provider,
        CredentialId {
            backend: backend.to_string(),
            index,
        },
        patch,
    )
}

fn request(model: &str) -> CompletionRequest {
    CompletionRequest::new(Vec::new(), ModelId::new(model), 256)
}

fn no_overrides() -> HashMap<String, ModelOverrideSpec> {
    HashMap::new()
}

// ---------------------------------------------------------------------------
// Failover chain semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn router_success_after_429_completes_on_backup() {
    let primary = StubProvider::new(
        "primary",
        vec![StubStep::Err(ProviderError::RateLimited {
            retry_after_ms: 30_000,
        })],
    );
    let backup = StubProvider::new("backup", vec![StubStep::Ok("served-by-backup")]);
    let primary_calls = Arc::clone(&primary);
    let backup_calls = Arc::clone(&backup);

    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![
                entry("primary", "m-primary", primary, 0, None),
                entry("backup", "m-backup", backup, 0, None),
            ],
        )],
        no_overrides(),
    )
    .expect("valid router");

    let resp = router
        .complete(request("default"))
        .await
        .expect("failover succeeds");
    assert_eq!(resp.content, "served-by-backup");
    assert_eq!(primary_calls.calls(), 1, "primary tried exactly once");
    assert_eq!(backup_calls.calls(), 1, "backup served");
    // Receipt honesty: the serving backend AND the failover path are marked.
    assert_eq!(router.name(), "primary->backup");
}

#[tokio::test]
async fn router_success_after_timeout_completes_on_backup() {
    let primary = StubProvider::new(
        "primary",
        vec![StubStep::Err(ProviderError::NetworkFailure(
            "connection timed out".to_string(),
        ))],
    );
    let backup = StubProvider::new("backup", vec![StubStep::Ok("after-timeout")]);

    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![
                entry("primary", "m-primary", primary, 0, None),
                entry("backup", "m-backup", backup, 0, None),
            ],
        )],
        no_overrides(),
    )
    .expect("valid router");

    let resp = router.complete(request("default")).await.expect("served");
    assert_eq!(resp.content, "after-timeout");
    assert_eq!(router.name(), "primary->backup");
}

#[tokio::test]
async fn router_non_retryable_error_stops_chain() {
    let primary = StubProvider::new(
        "primary",
        vec![StubStep::Err(ProviderError::InvalidRequest(
            "max_tokens out of range".to_string(),
        ))],
    );
    let backup = StubProvider::new("backup", vec![StubStep::Ok("must-not-serve")]);
    let backup_calls = Arc::clone(&backup);

    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![
                entry("primary", "m-primary", primary, 0, None),
                entry("backup", "m-backup", backup, 0, None),
            ],
        )],
        no_overrides(),
    )
    .expect("valid router");

    let err = router
        .complete(request("default"))
        .await
        .expect_err("non-retryable surfaces verbatim");
    assert!(
        matches!(err, ProviderError::InvalidRequest(ref m) if m.contains("max_tokens")),
        "the original typed error must surface, got {err:?}"
    );
    // Population pin: the backup was NEVER dispatched — the chain stopped.
    assert_eq!(backup_calls.calls(), 0);
    assert_eq!(router.name(), "router:failed(primary)");
}

#[tokio::test]
async fn router_upstream_5xx_advances_but_other_upstream_surfaces() {
    // 5xx (funneled): retryable — the backup serves.
    let sick = StubProvider::new(
        "sick",
        vec![StubStep::Err(ProviderError::Upstream(
            "HTTP 503: service unavailable".to_string(),
        ))],
    );
    let healthy = StubProvider::new("healthy", vec![StubStep::Ok("after-5xx")]);
    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![
                entry("sick", "m1", sick, 0, None),
                entry("healthy", "m2", healthy, 0, None),
            ],
        )],
        no_overrides(),
    )
    .expect("valid router");
    let resp = router.complete(request("default")).await.expect("served");
    assert_eq!(resp.content, "after-5xx");
    assert_eq!(router.name(), "sick->healthy");

    // Ambiguous upstream text: NOT retryable — the chain stops and the second
    // entry is provably never called.
    let confused = StubProvider::new(
        "confused",
        vec![StubStep::Err(ProviderError::Upstream(
            "decoding response body: invalid utf-8".to_string(),
        ))],
    );
    let untouched = StubProvider::new("untouched", vec![StubStep::Ok("must-not-serve")]);
    let untouched_calls = Arc::clone(&untouched);
    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![
                entry("confused", "m1", confused, 0, None),
                entry("untouched", "m2", untouched, 0, None),
            ],
        )],
        no_overrides(),
    )
    .expect("valid router");
    let err = router
        .complete(request("default"))
        .await
        .expect_err("ambiguous upstream surfaces");
    assert!(matches!(err, ProviderError::Upstream(_)), "{err:?}");
    assert_eq!(untouched_calls.calls(), 0);
}

#[tokio::test]
async fn router_empty_chain_is_a_typed_error_naming_the_lane() {
    let err = RouterProvider::new(
        "default",
        vec![("default".to_string(), Vec::new())],
        no_overrides(),
    )
    .err()
    .expect("an empty chain must not build");
    let msg = err.to_string();
    assert!(
        matches!(err, RouterError::EmptyLane { ref lane } if lane == "default"),
        "{msg}"
    );
    assert!(msg.contains("default"), "the error names the lane: {msg}");
}

#[tokio::test]
async fn router_missing_default_lane_is_a_typed_error() {
    let stub = StubProvider::new("only", vec![StubStep::Ok("x")]);
    let err = RouterProvider::new(
        "missing-default",
        vec![("other".to_string(), vec![entry("only", "m", stub, 0, None)])],
        no_overrides(),
    )
    .err()
    .expect("a router without its default lane must not build");
    let msg = err.to_string();
    assert!(
        matches!(err, RouterError::MissingDefaultLane(ref d) if d == "missing-default"),
        "{msg}"
    );
    assert!(msg.contains("missing-default"), "{msg}");
}

#[tokio::test]
async fn router_unknown_task_class_falls_back_to_default_and_logs_once() {
    let primary = StubProvider::new(
        "primary",
        vec![StubStep::Ok("lane-served"), StubStep::Ok("lane-served")],
    );
    let lane_stub = StubProvider::new("lane", vec![]);
    let lane_calls = Arc::clone(&lane_stub);

    let router = RouterProvider::new(
        "default",
        vec![
            (
                "default".to_string(),
                vec![entry("primary", "m-primary", primary, 0, None)],
            ),
            (
                "code".to_string(),
                vec![entry("lane", "m-code", lane_stub, 0, None)],
            ),
        ],
        no_overrides(),
    )
    .expect("valid router");

    // Two calls with an unknown class: both land on the default lane, and the
    // class is warned about exactly once (log-once, verified via the
    // diagnostics surface the single warning feeds).
    for _ in 0..2 {
        let resp = router
            .complete(request("mystery-class"))
            .await
            .expect("served");
        assert_eq!(resp.content, "lane-served");
    }
    assert_eq!(lane_calls.calls(), 0, "the 'code' lane was never used");
    assert_eq!(
        router.fallback_warned_classes(),
        vec!["mystery-class".to_string()],
        "exactly one warned class after two fallback calls"
    );
    // And the named lane itself routes normally.
    let resp = router.complete(request("code")).await.expect("served");
    assert_eq!(resp.content, "default-content");
    assert_eq!(lane_calls.calls(), 1);
}

// ---------------------------------------------------------------------------
// Receipt honesty
// ---------------------------------------------------------------------------

#[tokio::test]
async fn router_single_entry_lane_receipt_name_is_byte_identical() {
    // A single-entry, unpooled, unoverridden lane must leave receipts looking
    // exactly as if the backend were wired directly: the provider field is
    // the backend's plain name.
    let solo = StubProvider::new("anthropic", vec![StubStep::Ok("solo")]);
    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![entry("anthropic", "claude-opus-4-8", solo, 0, None)],
        )],
        no_overrides(),
    )
    .expect("valid router");

    assert_eq!(
        router.name(),
        "router",
        "before any call the name is static"
    );
    router.complete(request("default")).await.expect("served");
    assert_eq!(
        router.name(),
        "anthropic",
        "single-entry success renders byte-identically to today"
    );
    // The effective card is the provider's own, unretitled.
    assert_eq!(router.rate_card().version_id, "anthropic-v1");
}

#[tokio::test]
async fn router_all_entries_failed_marks_the_failed_path() {
    let a = StubProvider::new(
        "a",
        vec![StubStep::Err(ProviderError::RateLimited {
            retry_after_ms: 5_000,
        })],
    );
    let b = StubProvider::new(
        "b",
        vec![StubStep::Err(ProviderError::NetworkFailure(
            "down".to_string(),
        ))],
    );
    let a_calls = Arc::clone(&a);
    let b_calls = Arc::clone(&b);

    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![entry("a", "m1", a, 0, None), entry("b", "m2", b, 0, None)],
        )],
        no_overrides(),
    )
    .expect("valid router");

    let err = router
        .complete(request("default"))
        .await
        .expect_err("chain exhausted");
    // The LAST typed failure surfaces (network failure here).
    assert!(matches!(err, ProviderError::NetworkFailure(_)), "{err:?}");
    assert_eq!(router.name(), "router:failed(a->b)");

    // The rate-limited credential is now cooling: an immediate second call
    // dispatches nothing new against it.
    let _ = router.complete(request("default")).await;
    assert_eq!(a_calls.calls(), 1, "cooling credential skipped");
    assert_eq!(b_calls.calls(), 2, "uncooled entry retried");
}

// ---------------------------------------------------------------------------
// Credential pools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn router_credential_pool_rotates_on_429_and_skips_cooling_key_until_expiry() {
    // Two credentials on ONE backend: key0 429s, key1 serves.
    let key0 = StubProvider::new(
        "pool",
        vec![
            StubStep::Err(ProviderError::RateLimited {
                retry_after_ms: 120,
            }),
            StubStep::Ok("key0-after-cooldown"),
        ],
    );
    let key1 = StubProvider::new(
        "pool",
        vec![StubStep::Ok("key1-served"), StubStep::Ok("key1-served")],
    );
    let key0_calls = Arc::clone(&key0);
    let key1_calls = Arc::clone(&key1);

    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![
                entry("pool", "m", key0, 0, None),
                entry("pool", "m", key1, 1, None),
            ],
        )],
        no_overrides(),
    )
    .expect("valid router");

    // 1: injected 429 on key0 -> rotation to key1 wins.
    let resp = router.complete(request("default")).await.expect("served");
    assert_eq!(resp.content, "key1-served");
    assert_eq!(router.name(), "pool->pool#1", "rotation path marked");

    // 2: key0 is cooling (120ms), so the next call skips it — key1 again.
    let resp = router.complete(request("default")).await.expect("served");
    assert_eq!(resp.content, "key1-served");
    assert_eq!(key0_calls.calls(), 1, "cooling key skipped");
    assert_eq!(key1_calls.calls(), 2);

    // 3: after the cooldown expires, key0 is eligible again and serves.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let resp = router.complete(request("default")).await.expect("served");
    assert_eq!(resp.content, "key0-after-cooldown");
    assert_eq!(key0_calls.calls(), 2, "cooled credential rejoined");
}

#[tokio::test]
async fn router_unauthorized_advances_only_with_another_credential() {
    // With a different credential next in the chain, a 401 rotates.
    let dead = StubProvider::new("auth", vec![StubStep::Err(ProviderError::Unauthorized)]);
    let live = StubProvider::new("auth", vec![StubStep::Ok("second-credential")]);
    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![
                entry("auth", "m", dead, 0, None),
                entry("auth", "m", live, 1, None),
            ],
        )],
        no_overrides(),
    )
    .expect("valid router");
    let resp = router.complete(request("default")).await.expect("served");
    assert_eq!(resp.content, "second-credential");
    assert_eq!(router.name(), "auth->auth#1");

    // With NO other credential anywhere in the chain, the same 401 surfaces.
    let only = StubProvider::new("solo", vec![StubStep::Err(ProviderError::Unauthorized)]);
    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![entry("solo", "m", only, 0, None)],
        )],
        no_overrides(),
    )
    .expect("valid router");
    let err = router
        .complete(request("default"))
        .await
        .expect_err("single-credential 401 surfaces");
    assert!(matches!(err, ProviderError::Unauthorized), "{err:?}");
}

#[tokio::test]
async fn router_every_credential_cooling_surfaces_rate_limited() {
    let a = StubProvider::new(
        "a",
        vec![StubStep::Err(ProviderError::RateLimited {
            retry_after_ms: 60_000,
        })],
    );
    let b = StubProvider::new(
        "b",
        vec![StubStep::Err(ProviderError::RateLimited {
            retry_after_ms: 60_000,
        })],
    );
    let a_calls = Arc::clone(&a);
    let b_calls = Arc::clone(&b);
    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![entry("a", "m1", a, 0, None), entry("b", "m2", b, 0, None)],
        )],
        no_overrides(),
    )
    .expect("valid router");

    // First call cools both credentials and exhausts.
    let _ = router.complete(request("default")).await;
    assert_eq!(a_calls.calls(), 1);
    assert_eq!(b_calls.calls(), 1);

    // Now every credential is benched: the surface is RateLimited carrying the
    // earliest time a credential returns — and NOTHING is dispatched.
    let err = router
        .complete(request("default"))
        .await
        .expect_err("all cooling -> rate limited");
    match err {
        ProviderError::RateLimited { retry_after_ms } => {
            assert!(
                (1..=60_000).contains(&retry_after_ms),
                "retry_after should reflect the remaining cooldown, got {retry_after_ms}"
            );
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
    assert_eq!(a_calls.calls(), 1, "no dispatch while cooling");
    assert_eq!(b_calls.calls(), 1, "no dispatch while cooling");
    // Nothing was dispatched on the second call, so no backend may be named.
    assert_eq!(router.name(), "router:cooling");
}

// ---------------------------------------------------------------------------
// model_overrides
// ---------------------------------------------------------------------------

fn patch(micros_in: Option<u64>, micros_out: Option<u64>) -> ModelOverrideSpec {
    ModelOverrideSpec {
        context_window: None,
        input_price_micros: micros_in,
        output_price_micros: micros_out,
    }
}

#[tokio::test]
async fn router_model_override_changes_projection_and_absent_override_matches_baseline() {
    let usage = Usage {
        tokens_in: 1_000_000,
        tokens_out: 1_000_000,
        cost_cents: None,
    };

    // WITH override: input repriced at 3_000 micro-cents (0.003c) per 1k.
    let patched_stub = StubProvider::new("patched", vec![StubStep::Ok("x")]);
    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![entry(
                "patched",
                "m",
                patched_stub,
                0,
                Some(&patch(Some(3_000), None)),
            )],
        )],
        no_overrides(),
    )
    .expect("valid router");
    let overridden = router.rate_card().clone();
    assert_eq!(overridden.cents_per_1k_input, 0.003);
    // An un-named price keeps the provider's published value — nothing invented.
    assert_eq!(overridden.cents_per_1k_output, 1.5);
    assert!(
        overridden.version_id.ends_with("+override"),
        "overridden cards are auditable: {}",
        overridden.version_id
    );
    let with_override_cents = overridden.price(usage).cents;

    // WITHOUT override: the provider's own card, byte-identical math.
    let plain_stub = StubProvider::new("patched", vec![StubStep::Ok("x")]);
    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![entry("patched", "m", plain_stub, 0, None)],
        )],
        no_overrides(),
    )
    .expect("valid router");
    let baseline = router.rate_card().clone();
    assert_eq!(baseline.cents_per_1k_input, 0.3);
    assert_eq!(
        baseline.version_id, "patched-v1",
        "no retitle without a patch"
    );
    let baseline_cents = baseline.price(usage).cents;

    // Both directions pinned: the override CHANGES the projection (300c ->
    // 3c of input at 1M tokens) and its absence MATCHES the baseline card.
    assert_eq!(baseline_cents, 1800, "0.3c + 1.5c per 1k at 1M tokens");
    assert_eq!(
        with_override_cents, 1503,
        "0.003c + 1.5c per 1k at 1M tokens"
    );
    assert_ne!(with_override_cents, baseline_cents);
}

#[tokio::test]
async fn router_rate_card_reports_serving_entry_effective_card() {
    // After a failover, the card the runtime prices with is the SERVING
    // entry's — with that entry's override applied.
    let primary = StubProvider::new(
        "primary",
        vec![StubStep::Err(ProviderError::NetworkFailure(
            "down".to_string(),
        ))],
    );
    let backup = StubProvider::new("backup", vec![StubStep::Ok("served")]);
    let mut overrides = HashMap::new();
    overrides.insert("m-backup".to_string(), patch(Some(9_000), Some(99_000)));

    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![
                entry("primary", "m-primary", primary, 0, None),
                entry("backup", "m-backup", backup, 0, overrides.get("m-backup")),
            ],
        )],
        overrides,
    )
    .expect("valid router");

    assert_eq!(
        router.rate_card().version_id,
        "primary-v1",
        "pre-call basis"
    );
    router.complete(request("default")).await.expect("served");
    let card = router.rate_card();
    assert_eq!(card.version_id, "backup-v1+override");
    assert_eq!(card.cents_per_1k_input, 0.009);
    assert_eq!(card.cents_per_1k_output, 0.099);
}

#[tokio::test]
async fn router_override_naming_an_unknown_model_warns_loudly() {
    let stub = StubProvider::new("known", vec![StubStep::Ok("x")]);
    let mut overrides = HashMap::new();
    overrides.insert("m-known".to_string(), patch(Some(1_000), None));
    overrides.insert(
        "typo-model".to_string(),
        ModelOverrideSpec {
            context_window: Some(123_456),
            ..patch(None, None)
        },
    );
    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![entry("known", "m-known", stub, 0, overrides.get("m-known"))],
        )],
        overrides,
    )
    .expect("valid router");
    // The unknown key is surfaced (and warned at construction), not silent —
    // and the known key is NOT misreported.
    assert_eq!(
        router.unknown_override_models(),
        &["typo-model".to_string()]
    );
    // context_window rides the override table out to later lanes.
    assert_eq!(
        router.context_window(&ModelId::new("typo-model")),
        Some(123_456)
    );
    assert_eq!(router.context_window(&ModelId::new("m-known")), None);
}

#[tokio::test]
async fn router_completion_cost_reflects_the_effective_card() {
    // The completion response's cost is priced with the same effective card
    // the receipt path will use — never the unpatched provider card.
    let stub = StubProvider::new("priced", vec![StubStep::Ok("x")]);
    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![entry(
                "priced",
                "m",
                stub,
                0,
                Some(&patch(Some(3_000_000), None)), // 3c per 1k input
            )],
        )],
        no_overrides(),
    )
    .expect("valid router");
    let resp = router.complete(request("default")).await.expect("served");
    // 100 in / 50 out at 3c+1.5c per 1k = 0.3 + 0.075 = 0.375 -> ceil 1c.
    // Against the UNPATCHED card (0.3c/1k in) it would be 0.03+0.075 -> 1c too,
    // so pin the exact tuple against the effective card's own price() — the
    // values must be computed by the same card, not merely both nonzero.
    assert_eq!(resp.cost, router.rate_card().price(resp.usage));
    assert!(resp.cost.cents >= 1);
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn router_stream_failover_on_start_failure() {
    use futures::StreamExt;

    let primary = StubProvider::new(
        "primary",
        vec![StubStep::Err(ProviderError::RateLimited {
            retry_after_ms: 0,
        })],
    );
    let backup = StubProvider::new("backup", vec![StubStep::Ok("streamed-by-backup")]);
    let router = RouterProvider::new(
        "default",
        vec![(
            "default".to_string(),
            vec![
                entry("primary", "m1", primary, 0, None),
                entry("backup", "m2", backup, 0, None),
            ],
        )],
        no_overrides(),
    )
    .expect("valid router");

    let mut stream = router
        .stream(request("default"))
        .await
        .expect("stream starts");
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let Ok(ardur_provider_runtime::StreamEvent::ContentDelta(delta)) = item {
            text.push_str(&delta);
        }
    }
    assert_eq!(text, "streamed-by-backup");
    assert_eq!(router.name(), "primary->backup");
}

#[tokio::test]
async fn router_failover_path_survives_the_instrumented_wrapper() {
    // Production never holds a bare router: the CLI and server both wrap the
    // selected provider in `InstrumentedProvider`, and receipts read
    // `provider.name()` THROUGH that wrapper. If the wrapper did not forward
    // `name()`, every receipt would say the static string "router" and the
    // failover path would never be recorded. This pins the honest seam
    // end-to-end.
    use ardur_provider_runtime::InstrumentedProvider;

    let primary = StubProvider::new(
        "primary",
        vec![StubStep::Err(ProviderError::NetworkFailure(
            "down".to_string(),
        ))],
    );
    let backup = StubProvider::new("backup", vec![StubStep::Ok("served")]);
    let router: Arc<dyn Provider> = Arc::new(
        RouterProvider::new(
            "default",
            vec![(
                "default".to_string(),
                vec![
                    entry("primary", "m1", primary, 0, None),
                    entry("backup", "m2", backup, 0, None),
                ],
            )],
            no_overrides(),
        )
        .expect("valid router"),
    );
    let provider = InstrumentedProvider::wrap(router);

    provider.complete(request("default")).await.expect("served");
    assert_eq!(provider.id().0, "router");
    assert_eq!(
        provider.name(),
        "primary->backup",
        "the receipt-visible name must carry the failover path through the wrapper"
    );
}
