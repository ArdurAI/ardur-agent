//! **gh#413.** `file.write` captures prior content before destroying it.
//!
//! The unit tests in `snapshot.rs` prove the store round-trips. These prove
//! the store is actually *reached* by the write path — the reachability lesson
//! from ARD-459: a component that works but is never called is not a feature.

use std::collections::HashMap;
use std::path::PathBuf;

use ardur_tool_registry::snapshot::{Snapshot, SnapshotId, SnapshotStore};
use ardur_tool_registry::{CapTokenRef, InvocationId, SessionId, Tool, ToolContext, WriteFileTool};
use serde_json::json;

fn ctx() -> ToolContext {
    ToolContext {
        cap_token: CapTokenRef(String::new()),
        session_id: SessionId::new(),
        invocation_id: InvocationId::new(),
        cwd: PathBuf::from("."),
        env: HashMap::new(),
        cost_budget_cents: u32::MAX,
    }
}

/// An overwrite through `file.write` is recoverable.
#[tokio::test]
async fn a_write_captures_what_it_overwrites() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    tokio::fs::create_dir_all(&root).await.expect("root");
    tokio::fs::write(root.join("note.md"), b"original")
        .await
        .expect("seed");

    let store = SnapshotStore::new(dir.path().join("snapshots"));
    let tool = WriteFileTool::with_root(root.clone()).with_snapshots(store.clone());

    let out = tool
        .invoke(&ctx(), json!({ "path": "note.md", "content": "clobbered" }))
        .await
        .expect("the write succeeds");

    // The file really was overwritten.
    assert_eq!(
        tokio::fs::read(root.join("note.md")).await.expect("read"),
        b"clobbered"
    );

    // And the prior content is recoverable from the id the tool reported.
    let id = out.receipt_data["snapshot"]["prior_content"]
        .as_str()
        .expect("the write reports a snapshot id");
    let restored = store
        .read(&SnapshotId(id.to_string()))
        .await
        .expect("the snapshot is in the store");
    assert_eq!(
        restored, b"original",
        "the overwritten bytes must be recoverable"
    );
}

/// Creating a new file records that there was nothing to restore.
///
/// The id must be null rather than the digest of empty bytes: undoing a
/// creation means removing the file, and a digest would make it look like
/// there was prior content to put back.
#[tokio::test]
async fn creating_a_file_records_that_it_was_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    tokio::fs::create_dir_all(&root).await.expect("root");

    let store = SnapshotStore::new(dir.path().join("snapshots"));
    let tool = WriteFileTool::with_root(root.clone()).with_snapshots(store.clone());

    let out = tool
        .invoke(&ctx(), json!({ "path": "new.md", "content": "created" }))
        .await
        .expect("the write succeeds");

    assert!(
        out.receipt_data["snapshot"]["prior_content"].is_null(),
        "an absent file has no prior content: {}",
        out.receipt_data
    );

    // Restoring "absent" removes the file.
    store
        .restore(&Snapshot::NothingToCapture, &root.join("new.md"))
        .await
        .expect("restore succeeds");
    assert!(
        !root.join("new.md").exists(),
        "undoing a creation must remove the file"
    );
}

/// An append captures the pre-append content too.
///
/// Append is not destructive in the overwrite sense, but the prior state is
/// still the thing an undo needs, and a partial append leaves a file that
/// matches neither before nor after.
#[tokio::test]
async fn an_append_also_captures_the_prior_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    tokio::fs::create_dir_all(&root).await.expect("root");
    tokio::fs::write(root.join("log.md"), b"line one\n")
        .await
        .expect("seed");

    let store = SnapshotStore::new(dir.path().join("snapshots"));
    let tool = WriteFileTool::with_root(root.clone()).with_snapshots(store.clone());

    let out = tool
        .invoke(
            &ctx(),
            json!({ "path": "log.md", "content": "line two\n", "mode": "append" }),
        )
        .await
        .expect("the append succeeds");

    assert_eq!(
        tokio::fs::read(root.join("log.md")).await.expect("read"),
        b"line one\nline two\n"
    );

    let id = out.receipt_data["snapshot"]["prior_content"]
        .as_str()
        .expect("the append reports a snapshot id");
    let restored = store
        .read(&SnapshotId(id.to_string()))
        .await
        .expect("the snapshot is in the store");
    assert_eq!(
        restored, b"line one\n",
        "the pre-append content must be recoverable"
    );
}

/// Without a store configured, the tool behaves exactly as before.
///
/// The restrictive direction: snapshots are opt-in, so a deployment that has
/// not asked for them must see no behavioural change and no snapshot field.
#[tokio::test]
async fn without_a_store_the_write_is_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    tokio::fs::create_dir_all(&root).await.expect("root");
    tokio::fs::write(root.join("note.md"), b"original")
        .await
        .expect("seed");

    let tool = WriteFileTool::with_root(root.clone());
    let out = tool
        .invoke(&ctx(), json!({ "path": "note.md", "content": "clobbered" }))
        .await
        .expect("the write succeeds");

    assert_eq!(
        tokio::fs::read(root.join("note.md")).await.expect("read"),
        b"clobbered"
    );
    assert!(
        out.receipt_data.get("snapshot").is_none(),
        "no store means no snapshot claim: {}",
        out.receipt_data
    );
}

/// A full round trip: write, then put the file back the way it was.
#[tokio::test]
async fn an_overwrite_can_be_undone_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    tokio::fs::create_dir_all(&root).await.expect("root");
    let target = root.join("note.md");
    tokio::fs::write(&target, b"the original text")
        .await
        .expect("seed");

    let store = SnapshotStore::new(dir.path().join("snapshots"));
    let tool = WriteFileTool::with_root(root.clone()).with_snapshots(store.clone());

    let out = tool
        .invoke(
            &ctx(),
            json!({ "path": "note.md", "content": "a mistaken rewrite" }),
        )
        .await
        .expect("the write succeeds");

    let id = SnapshotId(
        out.receipt_data["snapshot"]["prior_content"]
            .as_str()
            .expect("a snapshot id")
            .to_string(),
    );
    store
        .restore(&Snapshot::Captured(id), &target)
        .await
        .expect("restore succeeds");

    assert_eq!(
        tokio::fs::read(&target).await.expect("read back"),
        b"the original text",
        "the mistaken write must be fully undone"
    );
}

/// Concurrent writes to the same path do not corrupt the store (review item 5).
///
/// Checked rather than asserted in prose. Two writes race between capture and
/// write, so the second capture may legitimately record the first write's
/// content — that is a semantics question, not corruption. What must hold is
/// that every id the tool reports resolves to bytes matching its digest, and
/// that the file ends as one of the two writes rather than interleaved.
#[tokio::test]
async fn concurrent_writes_leave_a_consistent_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    tokio::fs::create_dir_all(&root).await.expect("root");
    tokio::fs::write(root.join("hot.md"), b"initial")
        .await
        .expect("seed");

    let store = SnapshotStore::new(dir.path().join("snapshots"));
    let tool =
        std::sync::Arc::new(WriteFileTool::with_root(root.clone()).with_snapshots(store.clone()));

    let mut handles = Vec::new();
    for i in 0..8 {
        let tool = tool.clone();
        handles.push(tokio::spawn(async move {
            tool.invoke(
                &ctx(),
                json!({ "path": "hot.md", "content": format!("writer {i}") }),
            )
            .await
        }));
    }

    let mut ids = Vec::new();
    for h in handles {
        let out = h.await.expect("task joins").expect("the write succeeds");
        if let Some(id) = out.receipt_data["snapshot"]["prior_content"].as_str() {
            ids.push(id.to_string());
        }
    }

    assert!(!ids.is_empty(), "at least one write captured prior content");

    // Every reported id must resolve, and to bytes that hash to it — `read`
    // re-derives the digest, so a corrupt or truncated blob fails here.
    for id in &ids {
        store
            .read(&SnapshotId(id.clone()))
            .await
            .unwrap_or_else(|e| panic!("reported id `{id}` must resolve cleanly: {e}"));
    }

    // The file is exactly one writer's output, not a mix.
    let final_bytes = tokio::fs::read(root.join("hot.md")).await.expect("read");
    let final_text = String::from_utf8(final_bytes).expect("utf8");
    assert!(
        final_text.starts_with("writer ") && final_text.len() <= "writer 7".len(),
        "the file must be one writer's content, not interleaved: {final_text:?}"
    );
}
