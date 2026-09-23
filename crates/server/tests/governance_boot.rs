//! `governance_boot` — #502 Seam B7 follow-up: the server boot wires the
//! opt-in governance ER mirror (`ARDUR_GOVERNANCE` / `Config::governance_mirror`)
//! into the fused runtime.
//!
//! Contract inherited from #560's `governance_mirror` suite, asserted here at
//! the boot surface:
//!
//! - default OFF boots without creating `<data_dir>/governance/` and leaves
//!   binary allow/deny unchanged;
//! - ON + one committed `/chat` turn mints exactly one verifiable ER at
//!   `<data_dir>/governance/er-chain.jsonl`, signed by the SAME P-256 custody
//!   as native receipts;
//! - ON + a provider-failing (abandoned) turn mints nothing — the receipt
//!   chain may carry a cancellation marker, the mirror stays empty;
//! - ON + a corrupt mirror log fails the boot (fail-closed), never re-genesis.

mod support;

use std::sync::Arc;

use ardur_governance::{ErSigningKey, verify_er_log_lines};
use ardur_provider_runtime::{AnthropicProvider, ModelId, Provider, RateCard};
use ardur_receipt::Es256SigningKey;
use ardur_server::{AppState, GOVERNANCE_VERIFIER_ID, example_registry};
use serde_json::json;

/// Boot the stub-backed server for `config`, driving the worker to a clean
/// settle before the assertions read disk (the mirror append happens at the
/// commit decision, inside the turn's durable commit).
async fn boot(config: &ardur_server::Config) -> Arc<AppState> {
    support::boot_stub(config).await
}

/// Boot with a custom provider (the erroring backend for the abandoned turn).
async fn boot_with(config: &ardur_server::Config, provider: Arc<dyn Provider>) -> Arc<AppState> {
    let tools = Arc::new(example_registry("stub", "in-memory"));
    AppState::boot(
        config,
        provider,
        tools,
        ardur_fused_runtime::SharedDenyList::new(),
    )
    .await
    .expect("AppState boots")
}

/// The ER mirror log path under the booted data dir.
fn mirror_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("governance").join("er-chain.jsonl")
}

/// Read the mirror log's non-empty lines (empty when the file does not exist).
fn mirror_lines(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(mirror_path(dir))
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The boot's own receipt custody key, loaded from the persisted PEM — the ER
/// chain must verify against the SAME key native receipts sign with.
fn booted_er_jwks(dir: &std::path::Path) -> ardur_receipt::Jwks {
    let pem = std::fs::read_to_string(dir.join("keys").join("receipt.pem")).expect("receipt.pem");
    let receipt_key = Es256SigningKey::from_pkcs8_pem(&pem).expect("receipt key parses");
    ErSigningKey::from_pkcs8_pem(&receipt_key.to_pkcs8_pem().expect("pem"))
        .expect("er signing key")
        .jwks()
}

/// POST one chat turn through the booted router and return the HTTP status.
async fn one_chat_turn(state: &Arc<AppState>, message: &str) -> axum::http::StatusCode {
    let router = ardur_server::build_router(Arc::clone(state));
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/chat")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", support::CHAT_TOKEN))
        .body(axum::body::Body::from(
            json!({ "message": message }).to_string(),
        ))
        .expect("request builds");
    let (status, _bytes) = support::oneshot(router, request).await;
    status
}

/// Default-off boot: no `governance/` directory is created, and the HTTP
/// surface still answers a committed turn (byte-identical behaviour, no
/// admission change).
#[tokio::test]
async fn default_off_boots_without_a_mirror() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);
    assert!(!config.governance_mirror, "precondition: default off");

    let state = boot(&config).await;
    assert_eq!(
        one_chat_turn(&state, "hello").await,
        axum::http::StatusCode::OK
    );

    state.finish_shutdown().await.expect("worker settles");
    assert!(
        !dir.path().join("governance").exists(),
        "a default boot must not create the governance mirror directory"
    );
}

/// ON + one committed turn: one ER per evaluated event lands at the DESIGN.md
/// convention — the committed round plus the turn's memory write (this boot
/// configures an in-memory backend, so stage 9 runs) — and both verify
/// end-to-end against the boot's own receipt custody. The boot's
/// dev-permissive policy permits only Submit/ToolInvoke, so the memory
/// control plane denies the write: the turn still commits (memory is
/// non-fatal) and the event ER now reports that denial honestly instead of
/// the write passing silently.
#[tokio::test]
async fn on_with_a_committed_turn_mints_one_verifiable_er() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = support::test_config(&dir, None);
    config.governance_mirror = true;

    let state = boot(&config).await;
    assert_eq!(
        one_chat_turn(&state, "mirror me").await,
        axum::http::StatusCode::OK
    );
    state.finish_shutdown().await.expect("worker settles");

    let lines = mirror_lines(dir.path());
    assert_eq!(
        lines.len(),
        2,
        "one ER per evaluated event: the committed round plus the memory write"
    );
    let chain = verify_er_log_lines(&lines, &booted_er_jwks(dir.path()))
        .expect("the ER verifies against the boot's receipt custody");
    assert_eq!(chain.len(), 2);
    let claims = chain[0].receipt();
    assert_eq!(claims.verifier_id, GOVERNANCE_VERIFIER_ID);
    assert!(claims.parent_receipt_hash.is_none(), "genesis ER");
    let memory_event = chain[1].receipt();
    assert_eq!(memory_event.tool, "memory.write");
    assert!(
        memory_event.step_id.starts_with("ev:"),
        "the memory write is a stable-identity evaluated event"
    );
    assert_eq!(
        memory_event.verdict,
        ardur_governance::Verdict::Violation,
        "the dev-permissive policy (Submit/ToolInvoke only) denies the memory write"
    );
    assert_eq!(
        memory_event.internal_denial_code.as_deref(),
        Some("memory_policy_denied")
    );
}

/// ON + an abandoned (provider-failing) turn: the mirror stays empty. The
/// native chain may append a cancellation marker; the mirror mints no ER for a
/// turn that never reached the commit decision.
#[tokio::test]
async fn on_with_an_abandoned_turn_mints_no_er() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = support::test_config(&dir, None);
    config.governance_mirror = true;

    // A provider that always fails: the turn is abandoned before the commit
    // decision, so no native receipt and no ER.
    struct AlwaysFail {
        rate_card: RateCard,
    }
    #[async_trait::async_trait]
    impl Provider for AlwaysFail {
        async fn complete(
            &self,
            _req: ardur_provider_runtime::CompletionRequest,
        ) -> Result<ardur_provider_runtime::CompletionResponse, ardur_provider_runtime::ProviderError>
        {
            Err(ardur_provider_runtime::ProviderError::Upstream(
                "scripted provider error".to_string(),
            ))
        }

        fn id(&self) -> ardur_runtime::ProviderId {
            ardur_runtime::ProviderId("always-fail".to_string())
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        fn rate_card(&self) -> &RateCard {
            &self.rate_card
        }
    }
    let state = boot_with(
        &config,
        Arc::new(AlwaysFail {
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }),
    )
    .await;
    let status = one_chat_turn(&state, "doomed").await;
    assert_eq!(status, axum::http::StatusCode::BAD_GATEWAY);
    state.finish_shutdown().await.expect("worker settles");

    // The native receipt chain exists (boot laid it down) but the mirror
    // minted nothing for the abandoned turn.
    assert!(
        mirror_lines(dir.path()).is_empty(),
        "an abandoned turn must mint no ER"
    );
}

/// ON + a corrupt mirror log: the boot fails closed. The corrupt bytes are
/// left in place (no re-genesis), and the operator-facing error names the
/// ARDUR_GOVERNANCE knob.
#[tokio::test]
async fn on_with_a_corrupt_mirror_log_fails_the_boot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = support::test_config(&dir, None);
    config.governance_mirror = true;

    // Boot once so the layout exists, settle, then corrupt the mirror.
    {
        let state = boot(&config).await;
        state.finish_shutdown().await.expect("first boot settles");
    }
    let mirror = mirror_path(dir.path());
    assert!(
        mirror.exists(),
        "precondition: first boot created the mirror"
    );
    std::fs::write(&mirror, b"not-a-jws\n").expect("corrupt the mirror");

    let tools = Arc::new(example_registry("stub", "in-memory"));
    let provider: Arc<dyn Provider> =
        Arc::new(AnthropicProvider::stub(ModelId::new(&config.model)));
    let err = AppState::boot(
        &config,
        provider,
        tools,
        ardur_fused_runtime::SharedDenyList::new(),
    )
    .await
    .err()
    .expect("a corrupt mirror log must fail the boot");
    let message = format!("{err:#}");
    assert!(
        message.contains("ARDUR_GOVERNANCE"),
        "the boot error must name the knob: {message}"
    );
    assert_eq!(
        std::fs::read_to_string(&mirror).expect("corrupt bytes readable"),
        "not-a-jws\n",
        "fail-closed: the corrupt log is not overwritten"
    );
}
