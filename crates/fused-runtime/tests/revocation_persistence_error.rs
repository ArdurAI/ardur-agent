//! Failed durable revocation must never be acknowledged by the runtime.

mod support;

use std::sync::Arc;

use ardur_fused_runtime::SharedDenyList;
use ardur_runtime::{CapTokenRef, RuntimeError, SessionId};

#[tokio::test]
async fn runtime_does_not_acknowledge_failed_revocation_persistence() {
    let dir = support::tempdir().expect("fixture");
    let path = dir.path().join("deny.list");
    let deny = SharedDenyList::open_file(&path).expect("open durable list");
    let runtime = support::runtime_builder(Arc::new(support::EchoProvider::new()))
        .deny_list(deny)
        .build()
        .expect("runtime");
    // An explicit I/O failure independent of uid or chmod behavior: the path
    // ceases to be a writable regular file after the runtime has been built.
    std::fs::remove_file(&path).expect("remove fixture file");
    std::fs::create_dir(&path).expect("replace with directory");
    let result = runtime
        .revoke_cap_token(
            SessionId::new(),
            CapTokenRef(support::valid_token()),
            "test revocation",
        )
        .await;
    match result {
        Err(RuntimeError::Internal(error)) => assert!(
            error.to_string().contains("persisting revocation failed"),
            "the persistence error must reach the caller"
        ),
        _ => panic!("failed persistence must not acknowledge revocation or run success hooks"),
    }
}
