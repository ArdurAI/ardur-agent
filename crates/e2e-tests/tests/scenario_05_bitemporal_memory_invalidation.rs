//! Scenario §2.E5 — `memory_bitemporal_invalidation_at_past_t`.
//!
//! Proves the §7.0 bi-temporal store honors *both* time axes at once: a later
//! correction (a write on the transaction-time axis) never rewrites history on
//! the valid-time axis. A query "as of" a past instant still sees the value that
//! was live then, even after a newer fact has superseded it.
//!
//! The narrative: a user first prefers **tea** (fact F1, recorded at `t1`,
//! open-ended). Later, at `t2`, they switch to **coffee** (fact F2). The switch
//! is modelled as F1 being *invalidated* at `t2` while F2 carries the new live
//! value from `t2` forward. We then time-travel:
//!
//!   * at wall-clock *now* (`> t2`)   → coffee   (the current truth)
//!   * as of `t1.5` (between t1, t2)  → tea       (the pre-invalidation value)
//!   * as of `t1 - ε` (before t1)     → nothing    (F1 not yet valid)
//!   * as of exactly `t2`             → coffee     (the cutover instant)
//!   * **re-query** as of `t1.5`      → still tea   (the invalidation did not
//!     erase the past)
//!
//! ## Adapting to the actual §7.0 Phase-1 surface
//!
//! The scenario's placeholder ("F2 invalidates F1 *and* creates a new valid
//! version in the same chain") does not map onto the Phase-1 store verbatim, and
//! the divergence is itself worth pinning down:
//!
//!   * `invalidate(record_id, at, reason)` appends a *tombstone* row carrying
//!     `invalidation_time = at`; the original F1 row is **never mutated**
//!     (append-only — see `crates/memory/src/runtime.rs`). So "F1's record has
//!     `invalidation_time` set" is asserted against the appended tombstone in
//!     F1's correction chain, not against the pristine original row.
//!   * `at_time`'s cutoff is applied at the *chain* level — the earliest
//!     `invalidation_time` cuts off the **whole** `correction_chain_root` after
//!     that instant. A replacement fact that must remain live past the cutoff
//!     therefore lives in its **own** chain; the valid-time interval
//!     (`valid_from = t2`) carries the temporal hand-off. `invalidate()` records
//!     the transaction-time cutoff on the old chain. This is exactly the
//!     valid-time-vs-transaction-time separation the scenario sets out to prove.
//!
//! ## Why no on-disk session root
//!
//! Phase-1 ships only the in-process [`InMemoryMemoryRuntime`]; there is no
//! durable path to root at a `TempDir` yet (the `temp_session_root` fixture is
//! reserved for the `// TODO §7.0 Phase 2` pgvector-backed store). The bi-temporal
//! contract under test is the store's read/write semantics, which are fully
//! exercised in-process.

use ardur_memory::{
    HolderId, InMemoryMemoryRuntime, InvalidationReason, MemoryRecord, MemoryRuntime, RecordId,
    RecordKind, UnixTsMillis,
};
use serde_json::{Value, json};

/// Build a live preference fact for `subject`. `recorded_at` is pinned to the
/// `event_time`: in these deterministic scenarios the store learns of a fact at
/// the instant it happens, so the transaction-time axis tracks the event.
fn fact(
    subject: &HolderId,
    payload: Value,
    event_time: u64,
    valid_from: u64,
    valid_to: Option<u64>,
) -> MemoryRecord {
    MemoryRecord::new(
        subject.clone(),
        RecordKind::Preference,
        payload,
        UnixTsMillis(event_time),
        UnixTsMillis(valid_from),
        valid_to.map(UnixTsMillis),
        UnixTsMillis(event_time),
    )
}

/// The payloads `subject` holds valid at `as_of`, ordered by `valid_from` so the
/// assertions are deterministic regardless of insertion order.
fn payloads_at(rt: &InMemoryMemoryRuntime, subject: &HolderId, as_of: u64) -> Vec<Value> {
    let mut recs = rt.at_time(subject, UnixTsMillis(as_of));
    recs.sort_by_key(|r| r.valid_from);
    recs.into_iter().map(|r| r.payload).collect()
}

#[test]
fn tea_to_coffee_invalidation_honors_both_time_axes() {
    let rt = InMemoryMemoryRuntime::new();
    let user = HolderId::from("user:e2e-05");

    // The bi-temporal instants. `now` is the caller's wall clock (the store is
    // clock-agnostic — `current_as_of` reads whatever instant we hand it).
    let before_t1 = 999;
    let t1 = 1_000;
    let t1_5 = 1_500;
    let t2 = 2_000;
    let now = 3_000;

    // t1: the user prefers tea, open-ended (valid_to = ∞).
    let f1 = fact(&user, json!("tea"), t1, t1, None);
    let f1_id: RecordId = rt.record(f1).expect("F1 records");

    // At t1 the fact is already live (valid_from <= as_of).
    assert_eq!(payloads_at(&rt, &user, t1), vec![json!("tea")]);

    // t2: the user switches to coffee. The new fact is its own correction chain
    // (so it survives F1's chain-level cutoff) and is valid from t2 forward...
    let f2 = fact(&user, json!("coffee"), t2, t2, None);
    rt.record(f2).expect("F2 records");
    // ...and F1 is invalidated at t2 — append-only; the cutoff lands on F1's chain.
    rt.invalidate(f1_id, UnixTsMillis(t2), InvalidationReason::Superseded)
        .expect("F1 invalidates");

    // (4) Wall-clock now → coffee, the current truth.
    assert_eq!(
        payloads_at(&rt, &user, now),
        vec![json!("coffee")],
        "after the switch, the live preference is coffee"
    );
    // The `current_as_of` sugar agrees with `at_time` at the same instant.
    assert_eq!(
        rt.current_as_of(&user, UnixTsMillis(now)),
        rt.at_time(&user, UnixTsMillis(now)),
    );

    // (5) As of t1.5 (between t1 and t2) → tea, the pre-invalidation value.
    assert_eq!(
        payloads_at(&rt, &user, t1_5),
        vec![json!("tea")],
        "a query into the past still sees the value that was live then"
    );

    // (6) As of t1 - ε (before F1 was valid) → nothing.
    assert!(
        rt.at_time(&user, UnixTsMillis(before_t1)).is_empty(),
        "nothing is held valid before F1's valid_from"
    );

    // (7) As of exactly t2 (the cutover instant) → coffee. F1's chain cutoff is
    // `t2` and the cutoff is exclusive (`cutoff > as_of`), so F1 is already gone
    // at t2 while F2's interval has just opened (`valid_from <= as_of`).
    assert_eq!(
        payloads_at(&rt, &user, t2),
        vec![json!("coffee")],
        "at the cutover instant the new value is already live"
    );

    // The heart of bi-temporality: invalidating F1 at t2 is a *transaction-time*
    // event. Re-querying the *valid-time* past still returns tea — history was
    // appended to, not rewritten.
    assert_eq!(
        payloads_at(&rt, &user, t1_5),
        vec![json!("tea")],
        "the invalidation did not erase the past"
    );

    // (8) F1's `invalidation_time`: carried by the appended tombstone, not the
    // pristine original. The original row is byte-for-byte what we recorded.
    let history = rt.history_of(f1_id);
    assert_eq!(
        history.len(),
        2,
        "F1's chain is the original plus one tombstone"
    );

    let original = history
        .iter()
        .find(|r| r.record_id == f1_id.0)
        .expect("the original F1 row is retained");
    assert_eq!(
        original.invalidation_time, None,
        "the original F1 row is never mutated"
    );
    assert_eq!(original.valid_to, None, "F1 was recorded open-ended");

    let tombstone = history
        .iter()
        .find(|r| r.invalidation_time.is_some())
        .expect("an invalidation tombstone was appended");
    assert_eq!(
        tombstone.invalidation_time,
        Some(UnixTsMillis(t2)),
        "the cutoff lands at ~t2"
    );
    assert_eq!(
        tombstone.correction_chain_root, original.correction_chain_root,
        "the tombstone inherits F1's correction chain"
    );
}

/// A three-version chain F1 → F2 → F3 invalidated at three distinct instants. A
/// historical query at each window resolves to exactly the version that was live
/// then — the bi-temporal "as-of" read is stable across an arbitrarily long
/// correction history.
#[test]
fn chained_invalidations_resolve_the_right_version_at_each_instant() {
    let rt = InMemoryMemoryRuntime::new();
    let user = HolderId::from("user:e2e-05-chain");

    let t1 = 1_000;
    let t2 = 2_000;
    let t3 = 3_000;
    let now = 4_000;

    // Three successive preferences, each its own chain, each valid from its own
    // instant forward.
    let v1 = rt.record(fact(&user, json!("v1"), t1, t1, None)).unwrap();
    let v2 = rt.record(fact(&user, json!("v2"), t2, t2, None)).unwrap();
    let _v3 = rt.record(fact(&user, json!("v3"), t3, t3, None)).unwrap();

    // Each older version is superseded as the next arrives.
    rt.invalidate(v1, UnixTsMillis(t2), InvalidationReason::Superseded)
        .unwrap();
    rt.invalidate(v2, UnixTsMillis(t3), InvalidationReason::Superseded)
        .unwrap();

    // A point query in each window returns the single version live then.
    assert_eq!(payloads_at(&rt, &user, 1_500), vec![json!("v1")]);
    assert_eq!(payloads_at(&rt, &user, 2_500), vec![json!("v2")]);
    assert_eq!(payloads_at(&rt, &user, 3_500), vec![json!("v3")]);
    assert_eq!(payloads_at(&rt, &user, now), vec![json!("v3")]);
    assert!(rt.at_time(&user, UnixTsMillis(999)).is_empty());

    // The middle version's chain is the original plus its tombstone, cut at t3.
    let v2_history = rt.history_of(v2);
    assert_eq!(v2_history.len(), 2);
    assert_eq!(
        v2_history
            .iter()
            .filter_map(|r| r.invalidation_time)
            .collect::<Vec<_>>(),
        vec![UnixTsMillis(t3)],
    );
}

/// Invalidation is append-only and subject-scoped: it neither deletes the
/// superseded row nor leaks across subjects. After invalidating one user's fact,
/// the full lineage is still reconstructable and a different subject is unaffected.
#[test]
fn invalidation_is_append_only_and_subject_scoped() {
    let rt = InMemoryMemoryRuntime::new();
    let alice = HolderId::from("user:alice");
    let bob = HolderId::from("user:bob");

    let t1 = 1_000;
    let t2 = 2_000;

    let alice_f1 = rt.record(fact(&alice, json!("tea"), t1, t1, None)).unwrap();
    // Bob holds his own independent, never-invalidated preference.
    rt.record(fact(&bob, json!("water"), t1, t1, None)).unwrap();

    rt.invalidate(
        alice_f1,
        UnixTsMillis(t2),
        InvalidationReason::UserCorrection,
    )
    .unwrap();

    // Alice's superseded row survives in her history, pristine.
    let history = rt.history_of(alice_f1);
    assert_eq!(
        history.len(),
        2,
        "the original row is retained, not deleted"
    );
    let original = history
        .iter()
        .find(|r| r.record_id == alice_f1.0)
        .expect("alice's original row is intact");
    assert_eq!(*original, fact_as_stored(&alice, "tea", t1, original));

    // Bob is untouched by Alice's invalidation, at every instant.
    assert_eq!(payloads_at(&rt, &bob, t1), vec![json!("water")]);
    assert_eq!(payloads_at(&rt, &bob, 5_000), vec![json!("water")]);
    // And Alice has no live preference after her correction with no replacement.
    assert!(rt.at_time(&alice, UnixTsMillis(5_000)).is_empty());
}

/// Reconstruct what the original Alice row must look like, reusing the store's
/// own ids/chain-root from the retained row so the equality check pins every
/// *value* field (kind, payload, the three timestamps, the open valid_to, the
/// `None` invalidation_time) without hard-coding the random UUIDs.
fn fact_as_stored(
    subject: &HolderId,
    payload: &str,
    t: u64,
    stored: &MemoryRecord,
) -> MemoryRecord {
    let mut expected = fact(subject, json!(payload), t, t, None);
    expected.record_id = stored.record_id;
    expected.correction_chain_root = stored.correction_chain_root;
    expected
}
