use ardur_cap_token::{HolderId as CapHolderId, VerifiedClaims};
use ardur_cedar_policy::{CedarPolicyBundle, PolicyBundle, PolicySource};
use ardur_memory::{
    HolderId, InMemoryMemoryRuntime, MemoryAction, MemoryControlPlane, MemoryError, MemoryRecord,
    MemoryRuntime, ReceiptId, RecordKind, UnixTsMillis,
};

fn claims(subject: &str, tools: &[&str]) -> VerifiedClaims {
    VerifiedClaims {
        token_id: uuid::Uuid::new_v4(),
        audience: "ardur".to_string(),
        subject: CapHolderId(subject.to_string()),
        expires_unix: 2_000_000_000,
        budget_remaining: 100,
        tool_allowlist: tools.iter().map(|tool| (*tool).to_string()).collect(),
    }
}

fn policy(src: &str) -> CedarPolicyBundle {
    CedarPolicyBundle::load(PolicySource::Embedded(src.to_string())).expect("cedar policy")
}

fn record(subject: &str, object: &str, receipt_id: uuid::Uuid) -> MemoryRecord {
    let mut rec = MemoryRecord::new(
        HolderId::from(subject),
        RecordKind::Fact,
        serde_json::json!({
            "predicate": "remembers",
            "object": object,
            "source": "authorized-test",
            "workspace_id": subject,
            "confidence": 0.81,
        }),
        UnixTsMillis(1_000),
        UnixTsMillis(1_000),
        None,
        UnixTsMillis(1_000),
    );
    rec.source_receipt_id = Some(ReceiptId(receipt_id));
    rec
}

#[test]
fn authorized_memory_record_list_show_and_forget_are_cap_cedar_and_receipt_chained() {
    let runtime = InMemoryMemoryRuntime::new();
    let plane = MemoryControlPlane::new(&runtime, policy("permit(principal, action, resource);"));
    let claims = claims("workspace:alpha", &["memory.read", "memory.write"]);
    let receipt_id = uuid::Uuid::new_v4();

    let id = plane
        .record(
            &claims,
            record("workspace:alpha", "hybrid memory runbook", receipt_id),
        )
        .expect("authorized record succeeds");

    let listed = plane
        .list(
            &claims,
            &HolderId::from("workspace:alpha"),
            UnixTsMillis(2_000),
        )
        .expect("authorized list succeeds");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].receipt_id, Some(ReceiptId(receipt_id)));
    assert_eq!(listed[0].source.as_deref(), Some("authorized-test"));

    let shown = plane
        .show(&claims, &HolderId::from("workspace:alpha"), id)
        .expect("authorized show succeeds")
        .expect("record exists");
    assert_eq!(shown.scope.as_deref(), Some("workspace:alpha"));

    let forget_receipt = uuid::Uuid::new_v4();
    plane
        .forget(
            &claims,
            &HolderId::from("workspace:alpha"),
            id,
            UnixTsMillis(3_000),
            ReceiptId(forget_receipt),
        )
        .expect("authorized forget succeeds");

    let current = runtime.current_as_of(&HolderId::from("workspace:alpha"), UnixTsMillis(4_000));
    assert!(current.is_empty(), "forgotten memory is no longer current");
    let history = runtime.history_of(id);
    assert!(
        history.iter().any(|rec| {
            rec.invalidation_time == Some(UnixTsMillis(3_000))
                && rec.source_receipt_id == Some(ReceiptId(forget_receipt))
        }),
        "forget appends a receipt-chained tombstone with the forget receipt"
    );
}

#[test]
fn memory_control_plane_denies_missing_capability_cedar_deny_unreceipted_and_cross_workspace() {
    let runtime = InMemoryMemoryRuntime::new();
    let allow = MemoryControlPlane::new(&runtime, policy("permit(principal, action, resource);"));
    let read_only = claims("workspace:alpha", &["memory.read"]);
    let full = claims("workspace:alpha", &["memory.read", "memory.write"]);

    let err = allow
        .record(
            &read_only,
            record("workspace:alpha", "denied", uuid::Uuid::new_v4()),
        )
        .expect_err("missing memory.write denies");
    assert!(matches!(
        err,
        MemoryError::CapabilityDenied {
            action: MemoryAction::Record,
            ..
        }
    ));

    let mut unreceipted = record("workspace:alpha", "missing receipt", uuid::Uuid::new_v4());
    unreceipted.source_receipt_id = None;
    let err = allow
        .record(&full, unreceipted)
        .expect_err("unreceipted memory writes deny");
    assert!(matches!(
        err,
        MemoryError::ReceiptRequired {
            action: MemoryAction::Record
        }
    ));

    let err = allow
        .record(
            &full,
            record("workspace:other", "cross workspace", uuid::Uuid::new_v4()),
        )
        .expect_err("subject mismatch denies");
    assert!(matches!(err, MemoryError::SubjectMismatch { .. }));

    let deny = MemoryControlPlane::new(&runtime, policy("forbid(principal, action, resource);"));
    let err = deny
        .list(
            &full,
            &HolderId::from("workspace:alpha"),
            UnixTsMillis(2_000),
        )
        .expect_err("cedar deny blocks read");
    assert!(matches!(
        err,
        MemoryError::PolicyDenied {
            action: MemoryAction::List,
            ..
        }
    ));
}
