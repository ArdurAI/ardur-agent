//! Integration tests for the `ardur-admin` read-only HTTP surface.
//!
//! Fixtures write journals via the canonical
//! [`JournalEntry`](ardur_session_journals::JournalEntry) types and receipts as
//! synthetic compact JWS lines (only the base64url payload segment is read by
//! the loader), then drive the router in-process with `axum_test::TestServer`.

use std::fs;
use std::path::Path;

use ardur_admin::build_router;
use ardur_admin::state::AppState;
use ardur_session_journals::{
    CostDelta, CostTuple, JournalEntry, ReceiptId, ReservationId, UnixTsMillis,
};
use axum_test::TestServer;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use serde_json::Value;
use tempfile::TempDir;

// ---- fixtures -------------------------------------------------------------

fn user_msg(content: &str, at: UnixTsMillis) -> JournalEntry {
    JournalEntry::UserMessage {
        content: content.to_string(),
        at,
    }
}

fn assistant_msg(content: &str, at: UnixTsMillis) -> JournalEntry {
    JournalEntry::AssistantMessage {
        content: content.to_string(),
        at,
        receipt_id: ReceiptId::new(),
    }
}

fn cost_entry(cents: u64, at: UnixTsMillis) -> JournalEntry {
    let actual = CostTuple {
        tokens_in: 10,
        tokens_out: 20,
        cents,
        wall_ms: 100,
        attention_score: 0,
    };
    JournalEntry::CostFinalized {
        reservation_id: ReservationId::new(),
        actual,
        refunded: CostDelta {
            tokens_in: 0,
            tokens_out: 0,
            cents: 0,
            wall_ms: 0,
            attention_score: 0,
        },
        at,
    }
}

/// Write a session journal at `<journal_dir>/sessions/<id>/journal.jsonl`.
fn write_journal(journal_dir: &Path, session_id: &str, entries: &[JournalEntry]) {
    let dir = journal_dir.join("sessions").join(session_id);
    fs::create_dir_all(&dir).unwrap();
    let mut body = String::new();
    for e in entries {
        body.push_str(&serde_json::to_string(e).unwrap());
        body.push('\n');
    }
    fs::write(dir.join("journal.jsonl"), body).unwrap();
}

/// Append one synthetic receipt to `<receipt_store>/chain.jsonl`. The line is a
/// `header.payload.sig` compact JWS whose payload base64url-decodes to a
/// `ReceiptBody`; the loader only reads that middle segment.
#[allow(clippy::too_many_arguments)]
fn append_receipt(
    receipt_store: &Path,
    receipt_id: &str,
    verb: &str,
    issued_ms: u64,
    cents: u64,
    tokens_in: u64,
    tokens_out: u64,
    tools: &[&str],
) {
    let tool_calls: Vec<Value> = tools
        .iter()
        .map(|name| {
            serde_json::json!({
                "call_id": format!("call-{name}"),
                "tool_name": name,
                "arguments_digest": "0".repeat(64),
                "output_digest": "0".repeat(64),
                "cost": { "tokens_in": 0, "tokens_out": 0, "cents": 0, "wall_ms": 0, "attention_score": 0.0 },
            })
        })
        .collect();
    let body = serde_json::json!({
        "receipt_id": receipt_id,
        "parent_hash": null,
        "verb": verb,
        "issued_at": issued_ms,
        "subject": "user:test",
        "cap_token_id": "tok-test",
        "payload_digest": "0".repeat(64),
        "cost": {
            "tokens_in": tokens_in,
            "tokens_out": tokens_out,
            "cents": cents,
            "wall_ms": 250,
            "attention_score": 0.0
        },
        "tool_calls": tool_calls,
    });
    let payload = B64URL.encode(serde_json::to_vec(&body).unwrap());
    let line = format!("aGVhZGVy.{payload}.c2ln\n");
    fs::create_dir_all(receipt_store).unwrap();
    let path = receipt_store.join("chain.jsonl");
    let mut existing = fs::read_to_string(&path).unwrap_or_default();
    existing.push_str(&line);
    fs::write(&path, existing).unwrap();
}

/// Append one synthetic receipt carrying an explicit §11.14b `provider` field.
/// Mirrors [`append_receipt`] but adds the `"provider"` key so the loader's
/// field-over-verb preference can be exercised.
fn append_receipt_with_provider(
    receipt_store: &Path,
    receipt_id: &str,
    verb: &str,
    provider: &str,
    issued_ms: u64,
    cents: u64,
) {
    let body = serde_json::json!({
        "receipt_id": receipt_id,
        "parent_hash": null,
        "verb": verb,
        "issued_at": issued_ms,
        "subject": "user:test",
        "cap_token_id": "tok-test",
        "payload_digest": "0".repeat(64),
        "cost": {
            "tokens_in": 1,
            "tokens_out": 1,
            "cents": cents,
            "wall_ms": 250,
            "attention_score": 0.0
        },
        "provider": provider,
    });
    let payload = B64URL.encode(serde_json::to_vec(&body).unwrap());
    let line = format!("aGVhZGVy.{payload}.c2ln\n");
    fs::create_dir_all(receipt_store).unwrap();
    let path = receipt_store.join("chain.jsonl");
    let mut existing = fs::read_to_string(&path).unwrap_or_default();
    existing.push_str(&line);
    fs::write(&path, existing).unwrap();
}

/// A temp data dir with `journals/` and `receipts/` and a test server over it.
struct Fixture {
    _dir: TempDir,
    journal_dir: std::path::PathBuf,
    receipt_store: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let journal_dir = dir.path().join("journals");
        let receipt_store = dir.path().join("receipts");
        fs::create_dir_all(&journal_dir).unwrap();
        fs::create_dir_all(&receipt_store).unwrap();
        Self {
            _dir: dir,
            journal_dir,
            receipt_store,
        }
    }

    fn server(&self) -> TestServer {
        let state = AppState::new(&self.journal_dir, &self.receipt_store);
        TestServer::new(build_router(state.shared()))
    }
}

// ---- tests ----------------------------------------------------------------

#[tokio::test]
async fn healthz_returns_200() {
    let fx = Fixture::new();
    let res = fx.server().get("/healthz").await;
    res.assert_status_ok();
    assert_eq!(res.text(), "ok");
}

#[tokio::test]
async fn sessions_endpoint_lists_files() {
    let fx = Fixture::new();
    write_journal(
        &fx.journal_dir,
        "sess-a",
        &[
            user_msg("hi", UnixTsMillis::from(1_000u64)),
            assistant_msg("hello", UnixTsMillis::from(2_000u64)),
            cost_entry(7, UnixTsMillis::from(2_500u64)),
        ],
    );
    write_journal(
        &fx.journal_dir,
        "sess-b",
        &[user_msg("solo", UnixTsMillis::from(5_000u64))],
    );

    let res = fx.server().get("/api/sessions").await;
    res.assert_status_ok();
    let sessions: Value = res.json();
    let arr = sessions.as_array().unwrap();
    assert_eq!(arr.len(), 2, "both sessions listed");

    let by_id: std::collections::HashMap<&str, &Value> =
        arr.iter().map(|s| (s["id"].as_str().unwrap(), s)).collect();

    let a = by_id["sess-a"];
    assert_eq!(a["message_count"], 2, "user + assistant counted");
    assert_eq!(a["entry_count"], 3);
    assert_eq!(a["last_activity_ms"], 2_500);
    assert_eq!(a["last_cost_cents"], 7);

    let b = by_id["sess-b"];
    assert_eq!(b["message_count"], 1);
    assert_eq!(b["last_cost_cents"], Value::Null, "no cost settled");
}

#[tokio::test]
async fn journal_endpoint_returns_entries() {
    let fx = Fixture::new();
    write_journal(
        &fx.journal_dir,
        "sess-x",
        &[
            user_msg("first", UnixTsMillis::from(1u64)),
            assistant_msg("second", UnixTsMillis::from(2u64)),
        ],
    );

    let res = fx.server().get("/api/sessions/sess-x/journal").await;
    res.assert_status_ok();
    let page: Value = res.json();
    assert_eq!(page["session_id"], "sess-x");
    assert_eq!(page["total"], 2);
    assert_eq!(page["returned"], 2);
    let entries = page["entries"].as_array().unwrap();
    assert_eq!(entries[0]["kind"], "UserMessage");
    assert_eq!(entries[0]["content"], "first");
    assert_eq!(entries[1]["kind"], "AssistantMessage");
}

#[tokio::test]
async fn journal_endpoint_paginates() {
    let fx = Fixture::new();
    let entries: Vec<JournalEntry> = (0..5)
        .map(|i| user_msg(&format!("m{i}"), UnixTsMillis::from(i as u64)))
        .collect();
    write_journal(&fx.journal_dir, "sess-p", &entries);
    let server = fx.server();

    // Default: whole journal (well under the 100 default).
    let all: Value = server.get("/api/sessions/sess-p/journal").await.json();
    assert_eq!(all["total"], 5);
    assert_eq!(all["returned"], 5);

    // limit=2 with no offset → the *tail* (last two).
    let tail: Value = server
        .get("/api/sessions/sess-p/journal?limit=2")
        .await
        .json();
    assert_eq!(tail["total"], 5);
    assert_eq!(tail["returned"], 2);
    assert_eq!(tail["offset"], 3);
    let tail_entries = tail["entries"].as_array().unwrap();
    assert_eq!(tail_entries[0]["content"], "m3");
    assert_eq!(tail_entries[1]["content"], "m4");

    // Explicit offset pages forward from the start.
    let head: Value = server
        .get("/api/sessions/sess-p/journal?limit=2&offset=0")
        .await
        .json();
    assert_eq!(head["offset"], 0);
    assert_eq!(head["returned"], 2);
    let head_entries = head["entries"].as_array().unwrap();
    assert_eq!(head_entries[0]["content"], "m0");
    assert_eq!(head_entries[1]["content"], "m1");
}

#[tokio::test]
async fn receipts_endpoint_summarizes() {
    let fx = Fixture::new();
    append_receipt(
        &fx.receipt_store,
        "11111111-1111-4111-8111-111111111111",
        "llm.completion.minted.v1",
        1_000,
        42,
        100,
        200,
        &["echo", "health"],
    );

    let res = fx.server().get("/api/receipts").await;
    res.assert_status_ok();
    let arr: Value = res.json();
    let r = &arr.as_array().unwrap()[0];
    assert_eq!(r["receipt_id"], "11111111-1111-4111-8111-111111111111");
    assert_eq!(r["provider"], "llm.completion.minted.v1");
    assert_eq!(r["cents"], 42);
    assert_eq!(r["tokens_in"], 100);
    assert_eq!(r["tokens_out"], 200);
    assert_eq!(r["tool_call_count"], 2);
    let tools = r["tool_calls"].as_array().unwrap();
    assert_eq!(tools[0], "echo");
    assert_eq!(tools[1], "health");

    // And the single-receipt endpoint returns the full decoded body.
    let full: Value = fx
        .server()
        .get("/api/receipts/11111111-1111-4111-8111-111111111111")
        .await
        .json();
    assert_eq!(full["body"]["verb"], "llm.completion.minted.v1");
    assert!(full["jws_compact"].as_str().unwrap().contains('.'));

    // An unknown id is a 404.
    let missing = fx.server().get("/api/receipts/does-not-exist").await;
    assert_eq!(missing.status_code(), 404);
}

#[tokio::test]
async fn receipts_endpoint_prefers_provider_field_over_verb() {
    let fx = Fixture::new();
    // §11.14b receipt: explicit provider differs from the verb. The summary's
    // "provider" dimension must surface the field, not the verb.
    append_receipt_with_provider(
        &fx.receipt_store,
        "22222222-2222-4222-8222-222222222222",
        "llm.completion.minted.v1",
        "anthropic",
        1_000,
        7,
    );
    // Pre-§11.14b receipt: no provider field → falls back to the verb.
    append_receipt(
        &fx.receipt_store,
        "33333333-3333-4333-8333-333333333333",
        "llm.completion.minted.v1",
        2_000,
        3,
        1,
        1,
        &[],
    );

    let arr: Value = fx.server().get("/api/receipts").await.json();
    let by_id: std::collections::HashMap<&str, &Value> = arr
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["receipt_id"].as_str().unwrap(), r))
        .collect();

    assert_eq!(
        by_id["22222222-2222-4222-8222-222222222222"]["provider"], "anthropic",
        "explicit provider field is preferred over the verb"
    );
    assert_eq!(
        by_id["33333333-3333-4333-8333-333333333333"]["provider"], "llm.completion.minted.v1",
        "absent provider field falls back to the verb"
    );

    // The cost-by-provider roll-up groups by the same preferred key: the two
    // receipts share a verb but split into "anthropic" + the verb fallback.
    let report: Value = fx.server().get("/api/costs").await.json();
    let by_provider: std::collections::HashMap<&str, u64> = report["by_provider"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["provider"].as_str().unwrap(),
                p["cents"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(by_provider.get("anthropic"), Some(&7));
    assert_eq!(by_provider.get("llm.completion.minted.v1"), Some(&3));
}

#[tokio::test]
async fn costs_endpoint_aggregates_by_day() {
    let fx = Fixture::new();
    // Two receipts on 2020-09-13, one on 2020-09-14 (UTC).
    let day_a = 1_600_000_000_000; // 2020-09-13T12:26:40Z
    let day_b = day_a + 86_400_000; // next day
    append_receipt(
        &fx.receipt_store,
        &uuid(1),
        "llm.completion.minted.v1",
        day_a,
        10,
        1,
        1,
        &[],
    );
    append_receipt(
        &fx.receipt_store,
        &uuid(2),
        "llm.completion.minted.v1",
        day_a,
        5,
        1,
        1,
        &[],
    );
    append_receipt(
        &fx.receipt_store,
        &uuid(3),
        "llm.completion.minted.v1",
        day_b,
        8,
        1,
        1,
        &[],
    );

    let report: Value = fx.server().get("/api/costs").await.json();
    assert_eq!(report["total_cents"], 23);

    let by_day = report["by_day"].as_array().unwrap();
    assert_eq!(by_day.len(), 2, "two distinct days");
    // Most-recent day first.
    assert_eq!(by_day[0]["day"], "2020-09-14");
    assert_eq!(by_day[0]["cents"], 8);
    assert_eq!(by_day[0]["count"], 1);
    assert_eq!(by_day[1]["day"], "2020-09-13");
    assert_eq!(by_day[1]["cents"], 15);
    assert_eq!(by_day[1]["count"], 2);
}

#[tokio::test]
async fn costs_endpoint_aggregates_by_provider() {
    let fx = Fixture::new();
    append_receipt(
        &fx.receipt_store,
        &uuid(1),
        "llm.completion.minted.v1",
        1_000,
        30,
        1,
        1,
        &[],
    );
    append_receipt(
        &fx.receipt_store,
        &uuid(2),
        "llm.completion.minted.v1",
        2_000,
        20,
        1,
        1,
        &[],
    );
    append_receipt(
        &fx.receipt_store,
        &uuid(3),
        "tool.exec.done.v1",
        3_000,
        5,
        1,
        1,
        &[],
    );

    let report: Value = fx.server().get("/api/costs").await.json();
    let by_provider = report["by_provider"].as_array().unwrap();
    assert_eq!(by_provider.len(), 2, "two distinct verbs/providers");
    // Highest cents first.
    assert_eq!(by_provider[0]["provider"], "llm.completion.minted.v1");
    assert_eq!(by_provider[0]["cents"], 50);
    assert_eq!(by_provider[0]["count"], 2);
    assert_eq!(by_provider[1]["provider"], "tool.exec.done.v1");
    assert_eq!(by_provider[1]["cents"], 5);
}

#[tokio::test]
async fn dashboard_html_renders() {
    let fx = Fixture::new();
    write_journal(
        &fx.journal_dir,
        "sess-d",
        &[
            user_msg("hi", UnixTsMillis::from(1u64)),
            cost_entry(99, UnixTsMillis::from(2u64)),
        ],
    );
    append_receipt(
        &fx.receipt_store,
        &uuid(7),
        "llm.completion.minted.v1",
        1_000,
        12,
        5,
        6,
        &["echo"],
    );

    let res = fx.server().get("/").await;
    res.assert_status_ok();
    let html = res.text();
    assert!(html.contains("<!DOCTYPE html>"));
    assert!(html.contains("ardur-admin"));
    assert!(
        html.contains("id=\"dashboard\""),
        "refreshable fragment present"
    );
    assert!(
        html.contains("hx-trigger=\"every 5s\""),
        "HTMX auto-refresh wired"
    );
    assert!(html.contains("htmx.org"), "HTMX loaded from CDN");
    assert!(html.contains("Cost by provider"));
    assert!(html.contains("Recent sessions"));
    assert!(html.contains("Recent receipts"));
}

#[tokio::test]
async fn read_only_no_write_endpoints() {
    let fx = Fixture::new();
    let server = fx.server();

    // Every route is GET-only: a write method on a known path is 405 Method
    // Not Allowed (never a 2xx). There is no handler that mutates anything.
    for (method, path) in [
        ("POST", "/api/sessions"),
        ("PUT", "/api/receipts"),
        ("DELETE", "/api/sessions/x/journal"),
        ("PATCH", "/api/costs"),
        ("POST", "/"),
    ] {
        let res = match method {
            "POST" => server.post(path).await,
            "PUT" => server.put(path).await,
            "DELETE" => server.delete(path).await,
            "PATCH" => server.patch(path).await,
            _ => unreachable!(),
        };
        let code = res.status_code().as_u16();
        assert_eq!(
            code, 405,
            "{method} {path} must be rejected (405), got {code}"
        );
    }
}

/// A deterministic UUIDv4-shaped string for fixture receipt ids.
fn uuid(n: u8) -> String {
    format!("{n:08x}-0000-4000-8000-000000000000")
}
