//! #544 — the fused runtime's typed plane-outage consult and idempotent
//! reconnect replay, exercised through real turns.
//!
//! The plane is a wiremock stand-in speaking the pinned revision's wire
//! shapes (ArdurAI/ardur dev @ 9cd2f2a5): 200 PERMIT/DENY, 403
//! `passport_revoked`, 503 `kill_switch_active`, and the transport shapes.
//! These tests pin the runtime-side contract:
//!
//! 1. An authenticated DENY, a revocation, and a kill switch each REFUSE
//!    the turn through the existing refusal settlement — they never fall
//!    back and never mint the unreachable marker.
//! 2. The owner-authorized unavailable class (connection refused) proceeds
//!    under native governance and mints EXACTLY ONE
//!    `governance.plane.unreachable.v1` receipt per outage window.
//! 3. A reconnect after a response-unknown consult replays the missed
//!    window as an explained duplicate — the tool is NOT re-run and the
//!    cost is NOT double-debited.
//! 4. No plane configured: byte-identical behavior (turn proceeds).

use std::sync::Arc;
use std::time::Duration;

use ardur_fused_runtime::load_persisted_chain;
use ardur_governance::{KILL_SWITCH_ACTIVE, PASSPORT_REVOKED, PlaneClient};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use ardur_runtime::ChatRuntime as _;
mod support;
use support::{runtime_builder, user_request, valid_token};

fn registry_with(
    tools: Vec<Box<dyn ardur_tool_registry::Tool>>,
) -> Arc<ardur_tool_registry::ToolRegistry> {
    let mut registry = ardur_tool_registry::ToolRegistry::new();
    for tool in tools {
        registry.register(tool).expect("tool id is unique");
    }
    Arc::new(registry)
}

fn plane_client(server_uri: &str, journal: &std::path::Path) -> Arc<PlaneClient> {
    Arc::new(
        PlaneClient::open(
            server_uri,
            "test-token",
            "-----BEGIN PUBLIC KEY-----\nfake-root\n-----END PUBLIC KEY-----",
            journal,
        )
        .expect("plane client opens"),
    )
}

fn permit_mock() -> Mock {
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "PERMIT",
            "session_id": "s",
        })))
}

fn deny_mock() -> Mock {
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "DENY",
            "session_id": "s",
            "reason": "tool_not_allowed",
        })))
}

fn status_mock(status: u16, body: serde_json::Value) -> Mock {
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
}

fn echo_registry() -> Arc<ardur_tool_registry::ToolRegistry> {
    registry_with(vec![Box::new(ardur_tool_registry::EchoTool::new())])
}

fn runtime_with_plane(
    provider: Arc<dyn ardur_provider_runtime::Provider>,
    plane: Arc<PlaneClient>,
    receipts: &std::path::Path,
) -> ardur_fused_runtime::FusedRuntime {
    runtime_builder(provider)
        .receipt_log(receipts)
        .with_tools(echo_registry())
        .maybe_with_plane(Some(plane))
        .build()
        .expect("runtime builds")
}

/// A provider that requests the echo tool once, then stops.
struct OneToolThenStop {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ardur_provider_runtime::Provider for OneToolThenStop {
    async fn complete(
        &self,
        _request: ardur_provider_runtime::CompletionRequest,
    ) -> Result<ardur_provider_runtime::CompletionResponse, ardur_provider_runtime::ProviderError>
    {
        use ardur_provider_runtime::{CompletionResponse, FinishReason, Usage};
        use ardur_runtime::{CostTuple, ToolCall};
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            Ok(CompletionResponse {
                content: String::new(),
                finish_reason: FinishReason::ToolUse(vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "echo".to_string(),
                    arguments: json!({"msg": "hi"}),
                }]),
                usage: Usage::default(),
                cost: CostTuple::default(),
                raw_provider_response: None,
            })
        } else {
            Ok(CompletionResponse {
                content: "done".to_string(),
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
                cost: CostTuple::default(),
                raw_provider_response: None,
            })
        }
    }

    fn id(&self) -> ardur_runtime::ProviderId {
        ardur_runtime::ProviderId("one-tool".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &ardur_provider_runtime::RateCard {
        static CARD: std::sync::OnceLock<ardur_provider_runtime::RateCard> =
            std::sync::OnceLock::new();
        CARD.get_or_init(ardur_provider_runtime::RateCard::anthropic_2026_q2_v1)
    }
}

#[tokio::test]
async fn an_authenticated_plane_deny_refuses_the_turn_and_mints_no_marker() {
    let server = MockServer::start().await;
    deny_mock().mount(&server).await;
    let dir = support::tempdir().expect("tempdir");
    let plane = plane_client(&server.uri(), &dir.path().join("plane.jsonl"));
    let receipts = dir.path().join("receipts.jsonl");
    let runtime = runtime_with_plane(
        Arc::new(OneToolThenStop {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        plane,
        &receipts,
    );

    let err = runtime
        .submit(user_request("go", &valid_token()))
        .await
        .expect_err("the plane denial must refuse the turn");
    assert!(
        err.to_string()
            .contains("governance plane denied the tool call"),
        "expected the typed plane denial, got: {err}"
    );

    let chain = load_persisted_chain(&receipts).expect("chain loads");
    assert!(
        chain
            .iter()
            .all(|r| r.body.verb.as_str() != "governance.plane.unreachable.v1"),
        "a denial must never mint the unreachable marker"
    );
}

#[tokio::test]
async fn revocation_and_kill_switch_refuse_without_fallback() {
    for (status, body, needle) in [
        (
            403u16,
            json!({"error": PASSPORT_REVOKED}),
            "credential revoked",
        ),
        (
            503,
            json!({"error": KILL_SWITCH_ACTIVE}),
            "kill switch active",
        ),
    ] {
        let server = MockServer::start().await;
        status_mock(status, body).mount(&server).await;
        let dir = support::tempdir().expect("tempdir");
        let plane = plane_client(&server.uri(), &dir.path().join("plane.jsonl"));
        let receipts = dir.path().join("receipts.jsonl");
        let runtime = runtime_with_plane(
            Arc::new(OneToolThenStop {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            plane,
            &receipts,
        );
        let err = runtime
            .submit(user_request("go", &valid_token()))
            .await
            .expect_err("the typed plane denial must refuse the turn");
        assert!(
            err.to_string().contains(needle),
            "expected `{needle}`, got: {err}"
        );
        let chain = load_persisted_chain(&receipts).expect("chain loads");
        assert!(
            chain
                .iter()
                .all(|r| r.body.verb.as_str() != "governance.plane.unreachable.v1"),
            "a typed denial must never mint the unreachable marker"
        );
    }
}

#[tokio::test]
async fn the_unavailable_class_falls_back_under_native_governance_with_one_marker() {
    // No server at this URI: connection refused — the enumerated transport
    // shape carrying the #502 B5 fallback right.
    let dir = support::tempdir().expect("tempdir");
    let receipts = dir.path().join("receipts.jsonl");

    // THREE tool-requesting turns inside one outage window (the manual
    // clock is pinned, so the window cannot advance), served by ONE
    // runtime — the debounce state is per-runtime by design, matching one
    // long-lived server process per outage. Every turn's first provider
    // round requests the echo tool, so each turn consults the plane at its
    // tool gate, falls back under native governance, and exactly ONE
    // marker is minted for the window.
    let runtime = runtime_with_plane(
        Arc::new(ToolEveryFirstRound),
        plane_client("http://127.0.0.1:9", &dir.path().join("plane.jsonl")),
        &receipts,
    );
    for turn in 0..3 {
        runtime
            .submit(user_request(&format!("go #{turn}"), &valid_token()))
            .await
            .expect("the unavailable class proceeds under native governance");
    }

    let chain = load_persisted_chain(&receipts).expect("chain loads");
    let markers: Vec<_> = chain
        .iter()
        .filter(|r| r.body.verb.as_str() == "governance.plane.unreachable.v1")
        .collect();
    assert_eq!(
        markers.len(),
        1,
        "exactly one marker per outage window across {} fallback turns, got {}",
        3,
        markers.len()
    );
}

#[tokio::test]
async fn ambiguous_delivery_refuses_and_mints_no_marker() {
    // A request that times out mid-flight may have been delivered: it must
    // deny, not fall back, and mint nothing.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let dir = support::tempdir().expect("tempdir");
    let plane = plane_client(&server.uri(), &dir.path().join("plane.jsonl"));
    let receipts = dir.path().join("receipts.jsonl");
    let runtime = runtime_with_plane(
        Arc::new(OneToolThenStop {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        plane,
        &receipts,
    );
    let err = runtime
        .submit(user_request("go", &valid_token()))
        .await
        .expect_err("ambiguous delivery must refuse");
    assert!(
        err.to_string().contains("delivery ambiguous"),
        "expected the ambiguous-delivery denial, got: {err}"
    );
    let chain = load_persisted_chain(&receipts).expect("chain loads");
    assert!(
        chain
            .iter()
            .all(|r| r.body.verb.as_str() != "governance.plane.unreachable.v1"),
        "an ambiguous delivery must never mint the unreachable marker"
    );
}

#[tokio::test]
async fn a_permitting_plane_is_transparent_to_the_turn() {
    let server = MockServer::start().await;
    permit_mock().mount(&server).await;
    let dir = support::tempdir().expect("tempdir");
    let plane = plane_client(&server.uri(), &dir.path().join("plane.jsonl"));
    let receipts = dir.path().join("receipts.jsonl");
    let runtime = runtime_with_plane(
        Arc::new(OneToolThenStop {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        plane,
        &receipts,
    );
    runtime
        .submit(user_request("go", &valid_token()))
        .await
        .expect("a permitting plane changes nothing");
    let chain = load_persisted_chain(&receipts).expect("chain loads");
    assert!(
        chain
            .iter()
            .all(|r| r.body.verb.as_str() != "governance.plane.unreachable.v1"),
        "no outage → no marker"
    );
}

#[tokio::test]
async fn no_plane_configured_behaves_as_before() {
    let dir = support::tempdir().expect("tempdir");
    let receipts = dir.path().join("receipts.jsonl");
    let runtime = runtime_builder(Arc::new(OneToolThenStop {
        calls: std::sync::atomic::AtomicUsize::new(0),
    }))
    .receipt_log(&receipts)
    .with_tools(echo_registry())
    .build()
    .expect("runtime builds");
    runtime
        .submit(user_request("go", &valid_token()))
        .await
        .expect("no plane configured: unchanged behavior");
}

#[tokio::test]
async fn reconnect_replay_re_attests_the_missed_window_without_rerunning_the_tool() {
    let server = MockServer::start().await;
    // Phase 1: the consult times out (response unknown) — the turn refuses
    // (ambiguous), and the event stays in the plane journal's backlog.
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let dir = support::tempdir().expect("tempdir");
    let journal = dir.path().join("plane.jsonl");
    let receipts = dir.path().join("receipts.jsonl");
    let tool = CountingEcho::new();
    let tool_calls = tool.calls.clone();
    let runtime = runtime_with_counting_tool(
        Arc::new(OneToolThenStop {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        plane_client(&server.uri(), &journal),
        &receipts,
        tool,
    );
    let err = runtime
        .submit(user_request("go", &valid_token()))
        .await
        .expect_err("ambiguous delivery refuses");
    assert!(err.to_string().contains("delivery ambiguous"));
    assert_eq!(
        tool_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the tool never ran under the ambiguous consult"
    );

    // Phase 2 (reconnect): the plane is back and PERMITS. The backlog
    // replays exactly once — the missed consult is re-attested as an
    // explained duplicate; no tool re-runs (there is no tool access in the
    // client at all) and no second debit occurs (the plane debits nothing
    // here; the native cost gate remains the only debit authority).
    server.reset().await;
    permit_mock().mount(&server).await;
    let plane = plane_client(&server.uri(), &journal);
    let replayed = plane.replay_backlog().await.expect("replay completes");
    assert_eq!(replayed, 1, "exactly the missed consult replays");
    assert_eq!(
        tool_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "replay never re-runs the tool"
    );
    let replayed_again = plane.replay_backlog().await.expect("replay completes");
    assert_eq!(replayed_again, 0, "replay is idempotent");
}

/// An echo tool that counts invocations, so replay provably never re-runs.
struct CountingEcho {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl CountingEcho {
    fn new() -> Self {
        Self {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl ardur_tool_registry::Tool for CountingEcho {
    fn id(&self) -> ardur_tool_registry::ToolId {
        ardur_tool_registry::ToolId::new("echo")
    }

    fn schema(&self) -> &ardur_tool_registry::ToolSchema {
        static SCHEMA: std::sync::OnceLock<ardur_tool_registry::ToolSchema> =
            std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| ardur_tool_registry::EchoTool::new().schema().clone())
    }

    fn required_capabilities(&self) -> &[ardur_tool_registry::Capability] {
        &[]
    }

    async fn invoke(
        &self,
        _ctx: &ardur_tool_registry::ToolContext,
        args: serde_json::Value,
    ) -> Result<ardur_tool_registry::ToolOutput, ardur_tool_registry::ToolError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ardur_tool_registry::ToolOutput {
            content: args.clone(),
            cost: ardur_runtime::CostTuple::default(),
            receipt_data: args,
        })
    }
}

fn runtime_with_counting_tool(
    provider: Arc<dyn ardur_provider_runtime::Provider>,
    plane: Arc<PlaneClient>,
    receipts: &std::path::Path,
    tool: CountingEcho,
) -> ardur_fused_runtime::FusedRuntime {
    let registry = registry_with(vec![Box::new(tool)]);
    runtime_builder(provider)
        .receipt_log(receipts)
        .with_tools(registry)
        .maybe_with_plane(Some(plane))
        .build()
        .expect("runtime builds")
}

/// A provider that requests the echo tool on the first round of EVERY turn
/// (then stops on the tool's result) — unlike [`OneToolThenStop`], which
/// only ever requests a tool once across the whole runtime.
struct ToolEveryFirstRound;

#[async_trait::async_trait]
impl ardur_provider_runtime::Provider for ToolEveryFirstRound {
    async fn complete(
        &self,
        request: ardur_provider_runtime::CompletionRequest,
    ) -> Result<ardur_provider_runtime::CompletionResponse, ardur_provider_runtime::ProviderError>
    {
        use ardur_provider_runtime::{CompletionResponse, FinishReason, Usage};
        use ardur_runtime::{CostTuple, ToolCall};
        // A turn whose transcript already carries a tool result settles.
        let has_tool_result = request
            .messages
            .iter()
            .any(|m| m.role == ardur_runtime::Role::Tool);
        if has_tool_result {
            Ok(CompletionResponse {
                content: "done".to_string(),
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
                cost: CostTuple::default(),
                raw_provider_response: None,
            })
        } else {
            Ok(CompletionResponse {
                content: String::new(),
                finish_reason: FinishReason::ToolUse(vec![ToolCall {
                    id: format!("call-{:?}", request.request_id),
                    name: "echo".to_string(),
                    arguments: json!({"msg": "hi"}),
                }]),
                usage: Usage::default(),
                cost: CostTuple::default(),
                raw_provider_response: None,
            })
        }
    }

    fn id(&self) -> ardur_runtime::ProviderId {
        ardur_runtime::ProviderId("tool-every-turn".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &ardur_provider_runtime::RateCard {
        static CARD: std::sync::OnceLock<ardur_provider_runtime::RateCard> =
            std::sync::OnceLock::new();
        CARD.get_or_init(ardur_provider_runtime::RateCard::anthropic_2026_q2_v1)
    }
}
