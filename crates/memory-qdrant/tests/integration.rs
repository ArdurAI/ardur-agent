//! Live Qdrant integration tests for [`QdrantMemoryRuntime`].
//!
//! Each is `#[ignore]`d: it needs a live Qdrant, which CI lacks by default.
//! `#[ignore]` (not a silent env early-return) keeps them off the default suite,
//! so a skip reports as `ignored`, never a masked `passed` (#358). Run with
//! `-- --ignored`:
//!
//! ```text
//! docker run -p 6333:6333 -p 6334:6334 qdrant/qdrant
//! QDRANT_INTEGRATION_TEST=1 QDRANT_URL=http://localhost:6334 \
//!   cargo test -p ardur-memory-qdrant --test integration -- --ignored
//! ```
//!
//! Each test uses its own collection name so they do not collide when run in
//! parallel, and deletes that collection on entry for a clean slate.

use ardur_memory::{
    HolderId, InvalidationReason, MemoryRecord, MemoryRuntime, RecordKind, UnixTsMillis,
};
use ardur_memory_qdrant::{MemorySnapshot, QdrantMemoryConfig, QdrantMemoryRuntime};

/// The Qdrant config for `collection`. Endpoint from `QDRANT_URL` (default
/// `http://localhost:6334`); `#[ignore]` gates these tests, not an env
/// early-return, so a skip can never masquerade as a pass (#358).
fn config(collection: &str) -> QdrantMemoryConfig {
    QdrantMemoryConfig::from_env().with_collection_name(collection)
}

fn fact(subject: &str, payload: serde_json::Value, t: u64) -> MemoryRecord {
    MemoryRecord::new(
        HolderId::from(subject),
        RecordKind::Preference,
        payload,
        UnixTsMillis(t),
        UnixTsMillis(t),
        None,
        UnixTsMillis(t),
    )
}

/// Insert a record, then read it back via the bi-temporal "as-of" view.
#[test]
#[ignore = "requires a live Qdrant; run with `-- --ignored` (see module docs)"]
fn insert_then_query() {
    let cfg = config("ardur_it_insert_query");
    let rt = QdrantMemoryRuntime::connect(cfg).expect("connect");
    rt.delete_collection().ok();
    rt.init().expect("init");

    let user = HolderId::from("user:it-insert");
    rt.record(fact("user:it-insert", serde_json::json!("tea"), 1_000))
        .expect("record");

    let visible = rt.current_as_of(&user, UnixTsMillis(2_000));
    assert_eq!(visible.len(), 1, "the inserted fact is visible");
    assert_eq!(visible[0].payload, serde_json::json!("tea"));

    // A query before the fact's valid_from sees nothing.
    assert!(rt.at_time(&user, UnixTsMillis(500)).is_empty());

    rt.delete_collection().ok();
}

/// Invalidation cuts off the chain from the cutoff forward, but history is
/// retained and the pre-cutoff past is still readable.
#[test]
#[ignore = "requires a live Qdrant; run with `-- --ignored` (see module docs)"]
fn invalidate_preserves_history_and_past() {
    let cfg = config("ardur_it_invalidate");
    let rt = QdrantMemoryRuntime::connect(cfg).expect("connect");
    rt.delete_collection().ok();
    rt.init().expect("init");

    let user = HolderId::from("user:it-inval");
    let f1 = rt
        .record(fact("user:it-inval", serde_json::json!("tea"), 1_000))
        .expect("record f1");
    rt.record(fact("user:it-inval", serde_json::json!("coffee"), 2_000))
        .expect("record f2");
    rt.invalidate(f1, UnixTsMillis(2_000), InvalidationReason::Superseded)
        .expect("invalidate f1");

    // Now → coffee; mid-history → tea (the invalidation did not erase the past).
    let now: Vec<_> = rt
        .current_as_of(&user, UnixTsMillis(3_000))
        .into_iter()
        .map(|r| r.payload)
        .collect();
    assert_eq!(now, vec![serde_json::json!("coffee")]);
    let mid: Vec<_> = rt
        .at_time(&user, UnixTsMillis(1_500))
        .into_iter()
        .map(|r| r.payload)
        .collect();
    assert_eq!(mid, vec![serde_json::json!("tea")]);

    // F1's chain is the original plus its tombstone.
    let history = rt.history_of(f1);
    assert_eq!(history.len(), 2, "original + tombstone");
    assert!(
        history
            .iter()
            .any(|r| r.invalidation_time == Some(UnixTsMillis(2_000)))
    );

    let missing = rt.invalidate(
        ardur_memory::RecordId(uuid::Uuid::new_v4()),
        UnixTsMillis(9_999),
        InvalidationReason::Expired,
    );
    assert!(matches!(
        missing,
        Err(ardur_memory::MemoryError::NotFound(_))
    ));

    rt.delete_collection().ok();
}

/// The snapshot hook creates a Qdrant snapshot and records a `MemorySnapshot`
/// event on the receipt chain.
#[test]
#[ignore = "requires a live Qdrant; run with `-- --ignored` (see module docs)"]
fn snapshot_into_receipt_records_event() {
    let cfg = config("ardur_it_snapshot");
    let rt = QdrantMemoryRuntime::connect(cfg).expect("connect");
    rt.delete_collection().ok();
    rt.init().expect("init");
    rt.record(fact("user:it-snap", serde_json::json!("v1"), 1_000))
        .expect("record");

    let mut chain: Vec<MemorySnapshot> = Vec::new();
    let id = rt.snapshot_into_receipt(&mut chain).expect("snapshot");
    assert!(!id.is_empty(), "a snapshot id is returned");
    assert_eq!(chain.len(), 1, "one MemorySnapshot event on the chain");
    assert_eq!(chain[0].snapshot_id, id);
    assert!(chain[0].ts > 0, "the event carries a wall-clock timestamp");

    rt.delete_collection().ok();
}

/// The heart of durability: data written through one backend instance is still
/// readable after the instance is dropped and a fresh one reconnects to the same
/// collection — a simulated process restart.
#[test]
#[ignore = "requires a live Qdrant; run with `-- --ignored` (see module docs)"]
fn survives_simulated_restart() {
    let cfg = config("ardur_it_restart");
    let user = HolderId::from("user:it-restart");

    // First "process": write, then drop the whole backend (its client + runtime).
    {
        let rt = QdrantMemoryRuntime::connect(cfg.clone()).expect("connect");
        rt.delete_collection().ok();
        rt.init().expect("init");
        rt.record(fact("user:it-restart", serde_json::json!("durable"), 1_000))
            .expect("record");
        // `rt` dropped here — no in-process state carries over.
    }

    // Second "process": a fresh backend over the same collection sees the data.
    {
        let rt = QdrantMemoryRuntime::connect_and_init(cfg.clone()).expect("reconnect");
        let visible = rt.current_as_of(&user, UnixTsMillis(2_000));
        assert_eq!(visible.len(), 1, "the record survived the restart");
        assert_eq!(visible[0].payload, serde_json::json!("durable"));
        rt.delete_collection().ok();
    }
}
