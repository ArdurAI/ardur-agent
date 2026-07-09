//! Behavioral tests for §11.14 Phase 1: issue/verify roundtrip, append-only
//! attenuation narrowing, expiry, the tool allowlist, and revocation. Every
//! public error variant the verifier distinguishes is exercised.

use ardur_cap_token::{
    AttenuationRule, BiscuitCapTokenAttenuator, BiscuitCapTokenIssuer, BiscuitCapTokenVerifier,
    CapScope, CapToken, CapTokenAttenuator, CapTokenError, CapTokenIssuer, CapTokenVerifier,
    Caveat, FileDenyList, HashSetDenyList, HolderId, KeyPair, RequiredCaveats,
};

// Fixed timestamps keep the tests deterministic (no wall clock).
const ISSUE_EXPIRY: u64 = 2_000_000_000; // ~2033
const NOW: u64 = 1_700_000_000; // ~2023

fn scope(audience: &str, expires_unix: u64, budget: u64, tools: &[&str]) -> CapScope {
    CapScope {
        audience: audience.to_string(),
        expires_unix,
        budget_remaining: budget,
        tool_allowlist: tools.iter().map(|t| t.to_string()).collect(),
    }
}

fn request(now_unix: u64, audience: &str, tool: &str, cost: u64) -> RequiredCaveats {
    RequiredCaveats {
        now_unix,
        audience: audience.to_string(),
        tool: tool.to_string(),
        cost,
    }
}

fn issuer() -> BiscuitCapTokenIssuer {
    BiscuitCapTokenIssuer::new(KeyPair::new())
}

fn verifier() -> BiscuitCapTokenVerifier<HashSetDenyList> {
    BiscuitCapTokenVerifier::new(HashSetDenyList::new())
}

#[test]
fn issue_verify_roundtrip() {
    let issuer = issuer();
    let root = issuer.public_key();
    let token = issuer
        .issue(
            HolderId("spiffe://ardur/user/alice".to_string()),
            scope("svc-a", ISSUE_EXPIRY, 1000, &["search", "fetch"]),
        )
        .expect("issue");

    let verifier = verifier();
    let req = request(NOW, "svc-a", "search", 100);
    let claims = verifier.verify(&token, &root, &req).expect("verify");

    assert_eq!(claims.audience, "svc-a");
    assert_eq!(
        claims.subject,
        HolderId("spiffe://ardur/user/alice".to_string())
    );
    assert_eq!(claims.expires_unix, ISSUE_EXPIRY);
    assert_eq!(claims.budget_remaining, 1000);
    assert_eq!(
        claims.tool_allowlist,
        vec!["search".to_string(), "fetch".to_string()]
    );

    // The wire form round-trips and re-verifies against the same root.
    let wire = token.to_base64().expect("to_base64");
    let parsed = CapToken::from_base64(&wire, &root).expect("from_base64");
    let claims2 = verifier
        .verify(&parsed, &root, &req)
        .expect("verify parsed");
    assert_eq!(claims2.token_id, claims.token_id);

    // A different root rejects the signature.
    let other_root = KeyPair::new().public();
    assert!(matches!(
        verifier.verify(&token, &other_root, &req),
        Err(CapTokenError::SignatureInvalid)
    ));

    // A mismatched audience is distinguished from every other failure.
    let wrong_aud = request(NOW, "svc-b", "search", 100);
    assert!(matches!(
        verifier.verify(&token, &root, &wrong_aud),
        Err(CapTokenError::AudienceMismatch)
    ));
}

#[test]
fn attenuation_narrows() {
    let issuer = issuer();
    let root = issuer.public_key();
    let parent = issuer
        .issue(
            HolderId("alice".to_string()),
            scope("svc-a", ISSUE_EXPIRY, 1000, &["search", "fetch"]),
        )
        .expect("issue");

    let attenuator = BiscuitCapTokenAttenuator;
    let verifier = verifier();

    // ReduceBudget: a cost the parent allows but the child rejects.
    let child_budget = attenuator
        .attenuate(&parent, Caveat::from(AttenuationRule::ReduceBudget(100)))
        .expect("attenuate budget");
    let big = request(NOW, "svc-a", "search", 500);
    assert!(verifier.verify(&parent, &root, &big).is_ok());
    assert!(matches!(
        verifier.verify(&child_budget, &root, &big),
        Err(CapTokenError::BudgetExhausted)
    ));
    let small = request(NOW, "svc-a", "search", 50);
    assert!(verifier.verify(&child_budget, &root, &small).is_ok());

    // RestrictTools: parent allows `fetch`, the child only `search`.
    let child_tools = attenuator
        .attenuate(
            &parent,
            AttenuationRule::RestrictTools(vec!["search".to_string()]).into(),
        )
        .expect("attenuate tools");
    let fetch = request(NOW, "svc-a", "fetch", 10);
    assert!(verifier.verify(&parent, &root, &fetch).is_ok());
    assert!(matches!(
        verifier.verify(&child_tools, &root, &fetch),
        Err(CapTokenError::ToolNotAllowed)
    ));

    // EarlierExpiry: a time the parent accepts but the child has expired past.
    let child_exp = attenuator
        .attenuate(
            &parent,
            AttenuationRule::EarlierExpiry(1_500_000_000).into(),
        )
        .expect("attenuate expiry");
    let late = request(1_600_000_000, "svc-a", "search", 10);
    assert!(verifier.verify(&parent, &root, &late).is_ok());
    assert!(matches!(
        verifier.verify(&child_exp, &root, &late),
        Err(CapTokenError::Expired)
    ));

    // RestrictAudience: pinning to a foreign audience makes the token unusable
    // for the issued one (the two audience checks cannot both hold).
    let child_aud = attenuator
        .attenuate(
            &parent,
            AttenuationRule::RestrictAudience("svc-b".to_string()).into(),
        )
        .expect("attenuate audience");
    assert!(matches!(
        verifier.verify(&child_aud, &root, &request(NOW, "svc-a", "search", 10)),
        Err(CapTokenError::AudienceMismatch)
    ));
}

#[test]
fn verification_returns_effective_claims_after_attenuation() {
    let issuer = issuer();
    let root = issuer.public_key();
    let parent = issuer
        .issue(
            HolderId("alice".to_string()),
            scope("svc-a", ISSUE_EXPIRY, 1000, &["search", "fetch", "delete"]),
        )
        .expect("issue");

    let attenuator = BiscuitCapTokenAttenuator;
    let verifier = verifier();
    let child = attenuator
        .attenuate(&parent, AttenuationRule::ReduceBudget(800).into())
        .expect("attenuate first budget");
    let child = attenuator
        .attenuate(&child, AttenuationRule::ReduceBudget(250).into())
        .expect("attenuate second budget");
    let child = attenuator
        .attenuate(&child, AttenuationRule::EarlierExpiry(1_800_000_000).into())
        .expect("attenuate expiry");
    let child = attenuator
        .attenuate(
            &child,
            AttenuationRule::RestrictTools(vec!["fetch".to_string(), "delete".to_string()]).into(),
        )
        .expect("attenuate first tools");
    let child = attenuator
        .attenuate(
            &child,
            AttenuationRule::RestrictTools(vec!["fetch".to_string(), "shell".to_string()]).into(),
        )
        .expect("attenuate second tools");

    let claims = verifier
        .verify(&child, &root, &request(NOW, "svc-a", "fetch", 10))
        .expect("verify child");

    assert_eq!(claims.budget_remaining, 250);
    assert_eq!(claims.tool_allowlist, vec!["fetch".to_string()]);
}

#[test]
fn non_narrowing_budget_attenuation_cannot_widen_returned_claims() {
    let issuer = issuer();
    let root = issuer.public_key();
    let parent = issuer
        .issue(
            HolderId("alice".to_string()),
            scope("svc-a", ISSUE_EXPIRY, 1000, &["search"]),
        )
        .expect("issue");

    let child = BiscuitCapTokenAttenuator
        .attenuate(&parent, AttenuationRule::ReduceBudget(10_000).into())
        .expect("attenuate wider budget");
    let claims = verifier()
        .verify(&child, &root, &request(NOW, "svc-a", "search", 10))
        .expect("verify child");

    assert_eq!(claims.budget_remaining, 1000);
}

#[test]
fn expiry_rejected() {
    let issuer = issuer();
    let root = issuer.public_key();
    let token = issuer
        .issue(
            HolderId("alice".to_string()),
            scope("svc-a", 1_500_000_000, 1000, &["search"]),
        )
        .expect("issue");
    let verifier = verifier();

    // now > expiry → rejected.
    assert!(matches!(
        verifier.verify(
            &token,
            &root,
            &request(1_600_000_000, "svc-a", "search", 10)
        ),
        Err(CapTokenError::Expired)
    ));
    // now < expiry → accepted.
    assert!(
        verifier
            .verify(
                &token,
                &root,
                &request(1_400_000_000, "svc-a", "search", 10)
            )
            .is_ok()
    );
}

#[test]
fn tool_allowlist() {
    let issuer = issuer();
    let root = issuer.public_key();
    let token = issuer
        .issue(
            HolderId("alice".to_string()),
            scope("svc-a", ISSUE_EXPIRY, 1000, &["search", "fetch"]),
        )
        .expect("issue");
    let verifier = verifier();

    // Allowed tools pass.
    assert!(
        verifier
            .verify(&token, &root, &request(NOW, "svc-a", "search", 10))
            .is_ok()
    );
    assert!(
        verifier
            .verify(&token, &root, &request(NOW, "svc-a", "fetch", 10))
            .is_ok()
    );
    // A tool outside the allowlist is rejected.
    assert!(matches!(
        verifier.verify(&token, &root, &request(NOW, "svc-a", "delete", 10)),
        Err(CapTokenError::ToolNotAllowed)
    ));
}

#[test]
fn revocation() {
    let issuer = issuer();
    let root = issuer.public_key();
    let revoked = issuer
        .issue(
            HolderId("alice".to_string()),
            scope("svc-a", ISSUE_EXPIRY, 1000, &["search"]),
        )
        .expect("issue");
    let live = issuer
        .issue(
            HolderId("bob".to_string()),
            scope("svc-a", ISSUE_EXPIRY, 1000, &["search"]),
        )
        .expect("issue");

    let mut deny = HashSetDenyList::new();
    deny.revoke_token(&revoked);
    let verifier = BiscuitCapTokenVerifier::new(deny);

    let req = request(NOW, "svc-a", "search", 10);
    assert!(matches!(
        verifier.verify(&revoked, &root, &req),
        Err(CapTokenError::Revoked)
    ));
    // A separately issued token has distinct revocation ids and still verifies.
    assert!(verifier.verify(&live, &root, &req).is_ok());
}

#[test]
fn file_deny_list_persists_revocations_across_reopen() {
    let issuer = issuer();
    let root = issuer.public_key();
    let token = issuer
        .issue(
            HolderId("alice".to_string()),
            scope("svc-a", ISSUE_EXPIRY, 1000, &["search"]),
        )
        .expect("issue");
    let req = request(NOW, "svc-a", "search", 10);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cap-denylist.hex");

    let deny = FileDenyList::open(&path).expect("open deny list");
    assert!(
        BiscuitCapTokenVerifier::new(deny)
            .verify(&token, &root, &req)
            .is_ok(),
        "token starts live before any file-backed revocation"
    );

    let deny = FileDenyList::open(&path).expect("reopen deny list");
    deny.revoke_token(&token).expect("persist revocation");

    let reopened = FileDenyList::open(&path).expect("reopen persisted deny list");
    let verifier = BiscuitCapTokenVerifier::new(reopened);
    assert!(matches!(
        verifier.verify(&token, &root, &req),
        Err(CapTokenError::Revoked)
    ));
}

#[test]
fn file_deny_list_propagates_revocations_to_existing_verifiers() {
    let issuer = issuer();
    let root = issuer.public_key();
    let token = issuer
        .issue(
            HolderId("alice".to_string()),
            scope("svc-a", ISSUE_EXPIRY, 1000, &["search"]),
        )
        .expect("issue");
    let req = request(NOW, "svc-a", "search", 10);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cap-denylist.hex");

    let writer = FileDenyList::open(&path).expect("writer deny list");
    let reader = FileDenyList::open(&path).expect("reader deny list");
    let verifier = BiscuitCapTokenVerifier::new(reader);
    assert!(
        verifier.verify(&token, &root, &req).is_ok(),
        "reader verifier sees token as live before writer revokes it"
    );

    writer.revoke_token(&token).expect("persist revocation");

    assert!(matches!(
        verifier.verify(&token, &root, &req),
        Err(CapTokenError::Revoked)
    ));
}
