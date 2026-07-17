//! §5.0 Phase 1 — a sub-agent's authority is strictly narrower than its
//! parent's. Spawning with `RestrictTools([fs.read])` yields a cap-token that
//! authorizes `fs.read` but denies `fs.write`, even though the parent token
//! allowed both.

mod common;

use ardur_multi_agent::{
    AttenuationRule, BiscuitCapTokenVerifier, CapTokenError, CapTokenVerifier, HashSetDenyList,
    MultiAgentRuntime, RequiredCaveats,
};
use common::{AUDIENCE, NOW_UNIX, runtime_with, spec};

fn req(tool: &str) -> RequiredCaveats {
    RequiredCaveats {
        now_unix: NOW_UNIX,
        audience: AUDIENCE.to_string(),
        tool: tool.to_string(),
        cost: 1,
    }
}

#[tokio::test]
async fn restrict_tools_narrows_the_sub_agent_token() {
    // Parent may use both fs.read and fs.write.
    let (runtime, _parent_receipt_id, root) = runtime_with(&["fs.read", "fs.write"], 10_000);

    let handle = runtime
        .spawn(spec(
            "researcher-1",
            vec![AttenuationRule::RestrictTools(vec!["fs.read".to_string()])],
            10_000,
        ))
        .await
        .expect("spawn");

    // The sub-agent's attenuated cap-token, re-bound to the issuer root.
    let token = runtime
        .attenuated_token(&handle.agent_id)
        .expect("attenuated token");

    let verifier = BiscuitCapTokenVerifier::new(HashSetDenyList::new());

    // fs.read is still authorized.
    verifier
        .verify(&token, &root, &req("fs.read"))
        .expect("fs.read is allowed");

    // fs.write was attenuated away — denied.
    let err = verifier
        .verify(&token, &root, &req("fs.write"))
        .expect_err("fs.write is denied");
    assert!(matches!(err, CapTokenError::ToolNotAllowed));
}
