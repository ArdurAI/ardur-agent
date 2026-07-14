//! End-to-end seam test: mint a real `ardur_cap_token` grant, verify it,
//! project the governed tool-call into an Ardur Execution Receipt, sign + chain
//! it, verify the mirror chain, and derive the kernel-enforcement profile — all
//! through the crates' actual public APIs (no mocks, no invented surface).

use std::collections::BTreeMap;

use ardur_cap_token::{
    AttenuationRule, BiscuitCapTokenAttenuator, BiscuitCapTokenIssuer, BiscuitCapTokenVerifier,
    CapScope, CapToken, CapTokenAttenuator, CapTokenError, CapTokenIssuer, CapTokenVerifier,
    Caveat, HashSetDenyList, HolderId, KeyPair, RequiredCaveats,
};
use ardur_governance::{
    ActionClass, AuthOutcome, EnforceAction, EnforceMode, EnforceOp, EnforcementAttach,
    EnforcementProfile, ErSigner, ErSigningKey, ErVerifier, EvidenceLevel, GrantDescriptor,
    MissionRef, PublicDenialReason, RecordingAttach, SideEffectClass, StepContext, ToolInvocation,
    Verdict, project_execution_receipt, verify_er_chain,
};
use ardur_receipt::Es256SigningKey;
use serde_json::json;

const AUDIENCE: &str = "svc.tools";
const SUBJECT: &str = "spiffe://ardur.dev/agent/alice";
const VERIFIER_ID: &str = "spiffe://ardur.dev/verifier/fused-runtime-1";
const TRACE_ID: &str = "trace:run-0001";
const RUN_NONCE: &str = "cn5vbmNlLXYwMS1hYWFhYWFhYQ"; // base64url, 26 chars

fn issue_and_verify(
    tool: &str,
    tool_allowlist: Vec<String>,
    budget: u64,
    cost: u64,
) -> (
    Result<ardur_cap_token::VerifiedClaims, CapTokenError>,
    CapToken,
) {
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let scope = CapScope {
        audience: AUDIENCE.to_string(),
        expires_unix: 4_000_000_000,
        budget_remaining: budget,
        tool_allowlist,
    };
    let token = issuer
        .issue(HolderId(SUBJECT.to_string()), scope)
        .expect("issue cap-token");
    let verifier = BiscuitCapTokenVerifier::new(HashSetDenyList::new());
    let required = RequiredCaveats {
        now_unix: 1_700_000_000,
        audience: AUDIENCE.to_string(),
        tool: tool.to_string(),
        cost,
    };
    let claims = verifier.verify(&token, &issuer.public_key(), &required);
    (claims, token)
}

#[test]
fn compliant_tool_call_projects_a_verifiable_execution_receipt_chain() {
    let allow = vec![
        "shell.run".to_string(),
        "cap.shell_exec".to_string(),
        "cap.fs_read".to_string(),
        "cap.network_out".to_string(),
    ];
    let (claims, _token) = issue_and_verify("shell.run", allow, 1000, 10);
    let claims = claims.expect("cap-token verifies for an allowed tool");

    // The governed runtime signs ER with the SAME P-256 custody as native
    // receipts — one key, one JWKS. Prove kid parity across the two crates.
    let native_key = Es256SigningKey::generate();
    let pem = native_key.to_pkcs8_pem().unwrap();
    let er_key = ErSigningKey::from_pkcs8_pem(&pem).unwrap();
    assert_eq!(
        er_key.kid(),
        native_key.key_id(),
        "ER signer kid must match the native receipt key_id for the same PEM"
    );
    let jwks = er_key.jwks();

    let args = json!({ "cmd": "ls", "args": ["-la"], "cwd": "/work" });
    let call = ToolInvocation {
        tool: "shell.run",
        action_class: ActionClass::Read,
        target: "/work",
        resource_family: "filesystem",
        side_effect_class: SideEffectClass::None,
        arguments: &args,
    };

    // Root ER.
    let er0 = project_execution_receipt(
        &claims,
        &call,
        &AuthOutcome::Compliant,
        &StepContext {
            verifier_id: VERIFIER_ID,
            iss: VERIFIER_ID,
            trace_id: TRACE_ID,
            run_nonce: RUN_NONCE,
            step_id: "step-0",
            timestamp_millis: 1_700_000_000_000,
            ttl_secs: 300,
            evidence_level: EvidenceLevel::SelfSigned,
            parent: None,
        },
    )
    .expect("project root ER");
    assert_eq!(er0.verdict, Verdict::Compliant);
    assert_eq!(er0.grant_id, claims.token_id.to_string());
    assert_eq!(er0.actor, SUBJECT);
    assert!(er0.parent_receipt_id.is_none());
    assert!(er0.parent_receipt_hash.is_none());
    assert!(er0.public_denial_reason.is_none());
    assert!(er0.internal_denial_code.is_none());
    let signed0 = ErSigner::sign(er0, &er_key).unwrap();

    // Child ER chained off the root.
    let args1 = json!({ "cmd": "cat", "path": "/work/readme" });
    let call1 = ToolInvocation {
        tool: "shell.run",
        action_class: ActionClass::Read,
        target: "/work/readme",
        resource_family: "filesystem",
        side_effect_class: SideEffectClass::None,
        arguments: &args1,
    };
    let er1 = project_execution_receipt(
        &claims,
        &call1,
        &AuthOutcome::Compliant,
        &StepContext {
            verifier_id: VERIFIER_ID,
            iss: VERIFIER_ID,
            trace_id: TRACE_ID,
            run_nonce: RUN_NONCE,
            step_id: "step-1",
            timestamp_millis: 1_700_000_001_000,
            ttl_secs: 300,
            evidence_level: EvidenceLevel::SelfSigned,
            parent: Some(&signed0),
        },
    )
    .expect("project child ER");
    assert_eq!(
        er1.parent_receipt_hash.as_deref(),
        Some(signed0.receipt_hash().as_str())
    );
    assert_eq!(
        er1.parent_receipt_id.as_deref(),
        Some(&signed0.receipt_hash()[..16])
    );
    let signed1 = ErSigner::sign(er1, &er_key).unwrap();

    // The signed ER carries the MCEP typ and verifies under the shared JWKS.
    let roundtrip = ErVerifier::verify_compact(signed1.jws_compact(), &jwks).unwrap();
    assert_eq!(roundtrip.step_id, "step-1");

    // The mirror chain verifies (signature + linkage).
    verify_er_chain(std::slice::from_ref(&signed0), &jwks).unwrap();
    verify_er_chain(&[signed0.clone(), signed1.clone()], &jwks).expect("2-hop ER chain verifies");

    // A tampered/forked chain is rejected: swap the order.
    let broken = verify_er_chain(&[signed1, signed0], &jwks);
    assert!(broken.is_err(), "reordered chain must break linkage");

    // Schema shape: every required claim serializes; no unknown keys leak.
    let v = serde_json::to_value(roundtrip).unwrap();
    for key in [
        "receipt_id",
        "grant_id",
        "parent_receipt_id",
        "parent_receipt_hash",
        "actor",
        "verifier_id",
        "trace_id",
        "run_nonce",
        "step_id",
        "invocation_digest",
        "tool",
        "action_class",
        "target",
        "resource_family",
        "side_effect_class",
        "verdict",
        "evidence_level",
        "reason",
        "policy_decisions",
        "arguments_hash",
        "budget_remaining",
        "timestamp",
        "iss",
        "iat",
        "exp",
        "jti",
    ] {
        assert!(v.get(key).is_some(), "required ER claim `{key}` missing");
    }
    assert_eq!(v["action_class"], "read");
    assert_eq!(v["invocation_digest"]["canonicalization"], "jcs-rfc8785");
    // compliant ⇒ denial fields absent (schema allOf invariant).
    assert!(v.get("public_denial_reason").is_none());
    assert!(v.get("internal_denial_code").is_none());
}

#[test]
fn denied_tool_call_projects_a_violation_receipt_with_denial_vocabulary() {
    // Allowlist omits the requested tool → cap-token denies with ToolNotAllowed.
    let (claims_res, _token) =
        issue_and_verify("net.post", vec!["shell.run".to_string()], 1000, 10);
    let err = claims_res.expect_err("tool outside allowlist must be denied");
    assert!(matches!(err, CapTokenError::ToolNotAllowed));

    // We still need *some* verified claims to name the actor/grant on the
    // violation receipt; verify the same token against an allowed tool.
    let (claims_ok, _t) = issue_and_verify("shell.run", vec!["shell.run".to_string()], 1000, 10);
    let claims = claims_ok.unwrap();

    let outcome = AuthOutcome::from_cap_token_error(&err);
    let args = json!({ "url": "https://evil.example/exfil" });
    let call = ToolInvocation {
        tool: "net.post",
        action_class: ActionClass::Send,
        target: "https://evil.example/exfil",
        resource_family: "network",
        side_effect_class: SideEffectClass::ExternalSend,
        arguments: &args,
    };
    let er = project_execution_receipt(
        &claims,
        &call,
        &outcome,
        &StepContext {
            verifier_id: VERIFIER_ID,
            iss: VERIFIER_ID,
            trace_id: TRACE_ID,
            run_nonce: RUN_NONCE,
            step_id: "step-denied",
            timestamp_millis: 1_700_000_002_000,
            ttl_secs: 300,
            evidence_level: EvidenceLevel::SelfSigned,
            parent: None,
        },
    )
    .expect("project violation ER");

    assert_eq!(er.verdict, Verdict::Violation);
    assert_eq!(
        er.public_denial_reason,
        Some(PublicDenialReason::PolicyDenied)
    );
    assert_eq!(er.internal_denial_code.as_deref(), Some("tool_not_allowed"));

    let key = ErSigningKey::generate();
    let signed = ErSigner::sign(er, &key).unwrap();
    let claims_back = ErVerifier::verify_compact(signed.jws_compact(), &key.jwks()).unwrap();
    assert_eq!(claims_back.verdict, Verdict::Violation);
}

#[test]
fn enforcement_profile_mirrors_the_effective_capability_set() {
    // Grant allows exec + fs read + network, but NOT fs write.
    let allow = vec![
        "cap.shell_exec".to_string(),
        "cap.fs_read".to_string(),
        "cap.network_out".to_string(),
    ];
    let (claims, _t) = issue_and_verify("cap.shell_exec", allow, 1000, 1);
    let claims = claims.unwrap();

    let profile = EnforcementProfile::from_claims(
        "session-abc",
        &claims,
        "/work",
        vec!["10.0.0.0/8".to_string()],
        EnforceMode::Enforce,
    );

    let action_for = |op: EnforceOp| {
        profile
            .op_policies
            .iter()
            .find(|p| p.op == op)
            .map(|p| p.action)
            .unwrap()
    };
    assert_eq!(action_for(EnforceOp::Exec), EnforceAction::Allow);
    assert_eq!(action_for(EnforceOp::FileRead), EnforceAction::Allowlist);
    assert_eq!(
        action_for(EnforceOp::FileWrite),
        EnforceAction::Deny,
        "fs write absent from the grant ⇒ kernel-denied"
    );
    assert_eq!(action_for(EnforceOp::NetConnect), EnforceAction::Allowlist);
    assert_eq!(profile.path_allow, vec!["/work".to_string()]);

    // The daemon-request projection carries the numeric BpfOp/BpfAction codes.
    let req = profile.to_daemon_request_json();
    assert_eq!(req["session_id"], "session-abc");
    assert_eq!(req["enforce_mode"], 1);
    let exec = req["op_policies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["op"] == 0x01)
        .unwrap();
    assert_eq!(exec["action"], 0); // Allow

    // The attach seam records the applied profile (portable, no kernel).
    let attach = RecordingAttach::new();
    attach.apply(&profile).unwrap();
    assert_eq!(attach.applied().len(), 1);
    assert_eq!(attach.applied()[0].session_id, "session-abc");
}

#[test]
fn grant_descriptor_carries_the_cap_token_grant_and_mission_binding() {
    let allow = vec!["shell.run".to_string()];
    let (claims, _t) = issue_and_verify("shell.run", allow, 500, 1);
    let claims = claims.unwrap();

    let mission = MissionRef::Object {
        uri: "https://ardur.dev/missions/demo".to_string(),
        mission_digest: Some(format!("sha-256:{}", "0".repeat(64))),
    };
    let dg = GrantDescriptor::from_claims(&claims, Some(mission.clone()));
    assert_eq!(dg.grant_id, claims.token_id.to_string());
    assert_eq!(dg.subject, SUBJECT);
    assert_eq!(dg.budget_remaining, 500);
    assert_eq!(dg.mission_ref.as_ref().unwrap().uri(), mission.uri());

    // The descriptor is serializable for presentation to the Ardur proxy.
    let s = serde_json::to_string(&dg).unwrap();
    assert!(s.contains("grant_id"));
    assert!(s.contains("mission_ref") || s.contains("https://ardur.dev/missions/demo"));
}

#[test]
fn attenuated_grant_narrows_the_receipt_and_enforcement_authority() {
    // Issue broad, then attenuate to a strict subset; the verified claims (and
    // thus the ER budget + enforcement profile) reflect the narrowed authority.
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let scope = CapScope {
        audience: AUDIENCE.to_string(),
        expires_unix: 4_000_000_000,
        budget_remaining: 1000,
        tool_allowlist: vec!["shell.run".to_string(), "cap.fs_read".to_string()],
    };
    let token = issuer.issue(HolderId(SUBJECT.to_string()), scope).unwrap();
    let attenuator = BiscuitCapTokenAttenuator;
    let narrowed = attenuator
        .attenuate(&token, Caveat::new(AttenuationRule::ReduceBudget(50)))
        .unwrap();

    let verifier = BiscuitCapTokenVerifier::new(HashSetDenyList::default());
    let claims = verifier
        .verify(
            &narrowed,
            &issuer.public_key(),
            &RequiredCaveats {
                now_unix: 1_700_000_000,
                audience: AUDIENCE.to_string(),
                tool: "shell.run".to_string(),
                cost: 1,
            },
        )
        .unwrap();
    assert_eq!(
        claims.budget_remaining, 50,
        "attenuation lowered the ceiling"
    );

    let mut expect_budget = BTreeMap::new();
    expect_budget.insert("cost".to_string(), 50u64);
    let args = json!({});
    let call = ToolInvocation {
        tool: "shell.run",
        action_class: ActionClass::Read,
        target: "/x",
        resource_family: "filesystem",
        side_effect_class: SideEffectClass::None,
        arguments: &args,
    };
    let er = project_execution_receipt(
        &claims,
        &call,
        &AuthOutcome::Compliant,
        &StepContext {
            verifier_id: VERIFIER_ID,
            iss: VERIFIER_ID,
            trace_id: TRACE_ID,
            run_nonce: RUN_NONCE,
            step_id: "step-narrow",
            timestamp_millis: 1_700_000_003_000,
            ttl_secs: 300,
            evidence_level: EvidenceLevel::SelfSigned,
            parent: None,
        },
    )
    .unwrap();
    assert_eq!(er.budget_remaining, expect_budget);
}
