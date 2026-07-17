//! §5.1 real wire — happy path. A sub-agent narrowed to *just* `chat.submit`
//! still holds the capability a chat turn needs, so its turn is authorized at
//! the substrate and the echo response comes back. The attenuated token is
//! genuinely narrower than the parent (it carries an extra Biscuit block).

mod common;

use ardur_multi_agent::{AttenuationRule, MultiAgentRuntime};
use common::{ask, spec, verifying_runtime_with};

#[tokio::test]
async fn narrowed_but_authorized_sub_agent_submits_through_the_wire() {
    // Parent may chat and read; the sub-agent is narrowed to chat only.
    let (runtime, _parent_receipt_id, _root) =
        verifying_runtime_with(&["chat.submit", "fs.read"], 10_000);

    let handle = runtime
        .spawn(spec(
            "writer-1",
            vec![AttenuationRule::RestrictTools(vec![
                "chat.submit".to_string(),
            ])],
            10_000,
        ))
        .await
        .expect("spawn");

    // The turn is authorized (chat.submit survives the narrowing) and echoed.
    let resp = runtime
        .ask(&handle, ask("draft the intro", 25))
        .await
        .expect("authorized ask succeeds through the verifying wire");
    assert_eq!(resp.message.content, "draft the intro");

    // The reservation was charged against the envelope.
    assert_eq!(runtime.cents_used(&handle.agent_id), Some(25));

    // The sub-agent's authority is strictly narrower than the parent's: an
    // attenuation appends a Biscuit block, so the narrowed token carries more
    // than the parent's single authority-block revocation id.
    let token = runtime
        .attenuated_token(&handle.agent_id)
        .expect("attenuated token");
    assert!(
        token.revocation_ids().len() >= 2,
        "attenuated token must carry the appended narrowing block"
    );
}
