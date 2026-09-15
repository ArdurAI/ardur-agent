//! gh#361 / Epic 3: revocation must be durable and cross-process.
//!
//! The shared deny-list now has a file backend (`SharedDenyList::open_file`).
//! These tests pin the two acceptance properties:
//! - a revoked token stays revoked after the list is dropped and reopened
//!   (the restart case), and
//! - a revocation written by one handle is visible to an independently
//!   opened handle on the same path (the cross-process case).

use ardur_cap_token::{
    BiscuitCapTokenIssuer, CapScope, CapTokenIssuer, DenyList, HashSetDenyList, HolderId, KeyPair,
};
use ardur_fused_runtime::SharedDenyList;

fn mint_token() -> (ardur_cap_token::CapToken, Vec<Vec<u8>>) {
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let token = issuer
        .issue(
            HolderId("test-holder".to_string()),
            CapScope {
                audience: "ardur".to_string(),
                expires_unix: 2_000_000_000,
                budget_remaining: 1_000,
                tool_allowlist: ["chat.submit".to_string()].into_iter().collect(),
            },
        )
        .expect("issue token");
    let ids = token.revocation_ids();
    (token, ids)
}

#[test]
fn revocation_survives_drop_and_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deny.list");
    let (token, ids) = mint_token();

    {
        let list = SharedDenyList::open_file(&path).expect("open deny list");
        assert!(!list.is_revoked(&ids));
        list.revoke_token(&token).expect("revoke persists");
        assert!(list.is_revoked(&ids));
        // handle dropped at scope end — the process-restart analogue.
    }

    let reopened = SharedDenyList::open_file(&path).expect("reopen deny list");
    assert!(
        reopened.is_revoked(&ids),
        "a revoked token must still be revoked after the list is dropped and reopened"
    );
}

#[test]
fn revocation_is_visible_to_an_independently_opened_handle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deny.list");
    let (token, ids) = mint_token();

    let writer = SharedDenyList::open_file(&path).expect("writer handle");
    let reader = SharedDenyList::open_file(&path).expect("independent reader handle");

    writer.revoke_token(&token).expect("revoke persists");
    assert!(
        reader.is_revoked(&ids),
        "the independently opened handle must see the revocation (FileDenyList reloads on lookup)"
    );
}

#[test]
fn in_memory_backend_unchanged_and_process_local() {
    // The default backend keeps the old semantics: revoke is infallible and
    // visible only through clones of the same handle.
    let (token, ids) = mint_token();
    let list = SharedDenyList::new();
    assert!(!list.is_revoked(&ids));
    list.revoke_token(&token).expect("memory revoke");
    assert!(list.is_revoked(&ids));
    let other = SharedDenyList::new();
    assert!(
        !other.is_revoked(&ids),
        "a fresh in-memory handle must not see another handle's revocation"
    );
}

#[test]
fn memory_and_file_backends_agree_on_empty_lookup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deny.list");
    let file = SharedDenyList::open_file(&path).expect("open");
    let mem = SharedDenyList::new();
    let empty: Vec<Vec<u8>> = vec![];
    assert!(!file.is_revoked(&empty));
    assert!(!mem.is_revoked(&empty));
    // A HashSetDenyList baseline for parity with the pre-change behaviour.
    assert!(!HashSetDenyList::new().is_revoked(&empty));
}
