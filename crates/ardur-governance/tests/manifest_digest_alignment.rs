//! INTER-01 / #537 / F8 guard: the MD author and the ObservedEvent emitter
//! must produce the SAME manifest digest bytes for the same registry.
//!
//! Before the fix they diverged: `ardur_observed_events::manifest_digest`
//! emitted bare hex over id-only length-prefixed bytes, while
//! `ardur_governance::tool_manifest_digest` emitted `sha-256:`-prefixed hex
//! over (id, empty-descriptor) field pairs — `7eaa26b8…` vs `sha-256:78e583f0…`
//! for `["file.write"]`. Under verifier-contract §9.6 (plain string equality
//! against the declared digest) an unchanged registry would have read as
//! manifest drift forever.
//!
//! The canonical function is `ardur_core_types::tool_manifest_digest`:
//! sort, de-duplicate, u64-big-endian length-prefix each id, SHA-256, render
//! as `sha-256:`-prefixed lowercase hex. Both call sites delegate to it, and
//! this test pins the contract with golden vectors computed independently of
//! any of the three implementations — so a regression inside the shared
//! function itself cannot pass trivially by drifting both sides together.

use std::collections::BTreeMap;

use ardur_governance::{
    BudgetPair, EFFECT_CLASSES, GrantRecord, MissionIdentity, ToolManifestEntry,
    author_mission_declaration, tool_manifest_digest, tool_manifest_digest_of,
};

/// Golden vectors computed from the specification (sort → dedupe → u64-BE
/// length-prefix → SHA-256 → `sha-256:` prefix), NOT from any crate under
/// test. Verified against the pre-fix probe values: the canonical bytes are
/// the emitter's old bytes with the MD author's prefix.
const GOLDEN: &[(&[&str], &str)] = &[
    (
        &["file.write"],
        "sha-256:7eaa26b848621dd5a3f6efbd3a0b19b14c1489b81679bb2588f3d74dfe1fe50a",
    ),
    (
        &["file.read", "shell.run"],
        "sha-256:04a49c08da0e9b43ee7fe7a2c981c743e15867753fbeb827e40f8624ea75124e",
    ),
    (
        &["file.write", "file.read", "shell.run", "http.fetch"],
        "sha-256:c6e71303b89e313b8286c3d61da018894ffb02bc7988da133416fa521b62abad",
    ),
];

fn owned(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn both_call_sites_match_the_golden_vectors() {
    assert!(GOLDEN.len() >= 3, "the golden table must not be vacuous");
    for (ids, expected) in GOLDEN {
        let ids = owned(ids);
        let observed = ardur_observed_events::manifest_digest(&ids);
        let declared = tool_manifest_digest(&ids);
        let canonical = ardur_core_types::tool_manifest_digest(&ids);
        assert_eq!(
            &canonical, expected,
            "the canonical function for {ids:?} drifted from the pinned bytes"
        );
        assert_eq!(
            &observed, expected,
            "emitter digest for {ids:?} must equal the canonical bytes"
        );
        assert_eq!(
            &declared, expected,
            "MD-author digest for {ids:?} must equal the canonical bytes"
        );
    }
}

/// The §9.6 comparison is plain string equality across the two crates, so pin
/// equality on a table of registry shapes — orderings and duplicates
/// included — rather than on one lucky example.
#[test]
fn the_emitter_and_md_author_agree_across_registry_shapes() {
    let table: Vec<Vec<String>> = vec![
        owned(&["file.write"]),
        owned(&["file.read", "shell.run"]),
        // Enumeration order and duplicates must not read as drift.
        owned(&["shell.run", "file.read"]),
        owned(&["file.read", "file.read", "shell.run"]),
        owned(&["file.write", "file.read", "shell.run", "http.fetch"]),
    ];
    assert!(table.len() >= 5);
    let mut distinct = std::collections::BTreeSet::new();
    for ids in &table {
        let observed = ardur_observed_events::manifest_digest(ids);
        let declared = tool_manifest_digest(ids);
        assert_eq!(
            observed, declared,
            "§9.6 compares these strings; divergence is a false ManifestDrift"
        );
        assert_eq!(
            observed,
            ardur_core_types::tool_manifest_digest(ids),
            "both call sites must delegate to the one canonical function"
        );
        assert!(
            observed.starts_with("sha-256:") && observed.len() == "sha-256:".len() + 64,
            "the MD schema pins ^sha-256:[0-9a-f]{{64}}$: {observed}"
        );
        distinct.insert(observed);
    }
    // Content sensitivity: order/duplicate variants collapse, genuinely
    // different registries must not.
    assert_eq!(distinct.len(), 3, "the table holds three real registries");
}

/// The contract that actually ships: an MD authored from the operator's grant
/// ledger publishes a `tool_manifest_digest` that the emitter's observation of
/// the same registry matches byte-for-byte.
#[test]
fn an_authored_md_matches_the_emitters_observation_of_the_same_registry() {
    let identity = MissionIdentity {
        iss: "ardur-agent/cli".into(),
        sub: "cli://localhost".into(),
        aud: "ardur-governance-plane".into(),
        mission_id: "workspace://ardur-agent".into(),
        jti: "01a0adca-0000-4000-8000-00000000000e".into(),
        iat: 1_789_621_936,
        exp: 1_789_708_336,
        revocation_ref: "https://plane.local/revocations".into(),
    };
    let budgets: BTreeMap<String, BudgetPair> = EFFECT_CLASSES
        .iter()
        .map(|c| {
            (
                (*c).to_string(),
                BudgetPair {
                    ceiling: 100,
                    reserved_share: 10,
                },
            )
        })
        .collect();
    let grants = vec![
        GrantRecord {
            tool: "file.read".into(),
            capabilities: vec!["cap.fs_read".into()],
            scope: Some("/private/tmp/ardur-beta".into()),
            subject: "cli://localhost".into(),
            receipt_id: Some("receipt-1".into()),
        },
        GrantRecord {
            tool: "file.write".into(),
            capabilities: vec!["cap.fs_write".into()],
            scope: Some("/private/tmp/ardur-beta".into()),
            subject: "cli://localhost".into(),
            receipt_id: Some("receipt-1".into()),
        },
    ];
    let md = author_mission_declaration(&identity, &grants, &budgets).expect("MD authors");

    let observed = ardur_observed_events::manifest_digest(&owned(&["file.read", "file.write"]));
    assert_eq!(
        md.tool_manifest_digest, observed,
        "an unchanged registry must NOT read as §9.6 manifest drift"
    );
}

/// Descriptor pinning stays available to callers that can supply it, as a
/// strictly stronger pin: an entry with a descriptor digest must hash
/// DIFFERENTLY from the id-only canonical form (otherwise the descriptor
/// would be decorative), and two different descriptors must differ.
#[test]
fn a_descriptor_pin_is_a_stronger_distinct_digest() {
    let id_only = tool_manifest_digest(&["file.read".into()]);
    let pinned_a = tool_manifest_digest_of(&[ToolManifestEntry {
        id: "file.read".into(),
        descriptor_digest: Some("sha-256:aaa".into()),
    }]);
    let pinned_b = tool_manifest_digest_of(&[ToolManifestEntry {
        id: "file.read".into(),
        descriptor_digest: Some("sha-256:bbb".into()),
    }]);
    assert_ne!(id_only, pinned_a, "a descriptor pin must change the digest");
    assert_ne!(
        pinned_a, pinned_b,
        "a changed descriptor must change the digest"
    );

    // Entries without descriptors, even through the richer entry API, must
    // collapse to the canonical id-only bytes — that is what lets an MD
    // authored without registry access still match the emitter.
    let unpinned = tool_manifest_digest_of(&[
        ToolManifestEntry {
            id: "file.read".into(),
            descriptor_digest: None,
        },
        ToolManifestEntry {
            id: "shell.run".into(),
            descriptor_digest: None,
        },
    ]);
    assert_eq!(
        unpinned,
        tool_manifest_digest(&["file.read".into(), "shell.run".into()]),
        "descriptor-less entries must hash identically to the canonical form"
    );
}
