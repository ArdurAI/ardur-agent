//! Scenario §2.E6 — `subagent_cannot_escalate_attenuation`.
//!
//! Drives a three-generation sub-agent hierarchy — **parent → child →
//! grandchild** — through the §5.1 real wire (`CapVerifyingRuntime`) and the
//! §11.14 cap-token verifier, proving that authority only ever *narrows* down
//! the chain and that a descendant can never claw back a capability an ancestor
//! attenuated away.
//!
//! Unlike the per-crate multi-agent tests (which prove a single generation of
//! attenuation), this scenario chains three runtimes — each generation seeded
//! with the prior generation's *attenuated* token — so the narrowing composes
//! across generations exactly as a real delegation tree would.
//!
//! # The chain
//!
//! 1. **Parent** holds a root cap granting both `chat.submit` *and*
//!    `memory.write` for [`AUDIENCE`].
//! 2. **Child** is spawned by attenuating the parent cap with
//!    `RestrictTools([chat.submit])` — `memory.write` is dropped. The child runs
//!    a real, authorized `chat.submit` turn through the verifying wire.
//! 3. The child's lost `memory.write` is denied at the substrate the wire
//!    delegates to (the §11.14 verifier) — `ask` itself only ever exercises
//!    `chat.submit`, so the dropped-tool denial is proven one layer down.
//! 4. **Grandchild** is spawned by attenuating the *child's* cap with
//!    `EarlierExpiry` — a tighter time window. It runs its own authorized
//!    `chat.submit` turn, but cannot reach back to the broader window its
//!    ancestors still hold.
//! 5. **Escalation is rejected structurally.** The grandchild cannot re-grant
//!    the `memory.write` its parent dropped.
//!
//! # What "escalation rejection" actually looks like (the §5.1 finding)
//!
//! It is **not** a runtime error returned by `attenuate`, and **not** a compile
//! error for the specific re-listing attempt below. It is a *two-layer
//! structural impossibility*:
//!
//! - **Compile-time / type-level.** [`AttenuationRule`] exposes only narrowing
//!   constructors (`RestrictAudience` / `EarlierExpiry` / `ReduceBudget` /
//!   `RestrictTools`). There is no "add tool", "widen audience", "extend
//!   expiry", or "raise budget" variant — widening is *unrepresentable* in the
//!   type, so the most direct escalation cannot even be written.
//! - **Runtime / check-intersection.** The one variant that *names* a tool set,
//!   `RestrictTools`, does not validate the set against the parent: re-listing a
//!   dropped tool (`RestrictTools([chat.submit, memory.write])`) returns `Ok`
//!   from `attenuate`. But Biscuit appends a *check*, and the token's authority
//!   is the intersection of every block's checks — the ancestor block that
//!   pinned tools to `{chat.submit}` still rejects `memory.write`. So the
//!   re-listing is inert: the capability stays dropped.
//!
//! # A note on the audience axis
//!
//! The original backlog sketch narrowed the grandchild's *audience*. In this
//! single-audience Biscuit model that does not yield a usable "narrower
//! audience": `RestrictAudience(x)` appends `check if audience(x)` *on top of*
//! the issued `check if audience(AUDIENCE)`, and a single request supplies one
//! audience fact — so for any `x != AUDIENCE` the two checks can never both hold
//! and the token is bricked for every request (the cap-token suite's own
//! `attenuation_narrows` test documents this). The grandchild therefore narrows
//! on the **expiry** axis, where "operate inside the window / denied outside it"
//! is genuinely demonstrable.

use ardur_e2e_tests::fixtures::{AUDIENCE, dev_cap_issuer};

use ardur_cap_token::{
    AttenuationRule, BiscuitCapTokenAttenuator, BiscuitCapTokenVerifier, CapScope, CapToken,
    CapTokenAttenuator, CapTokenError, CapTokenIssuer, CapTokenVerifier, Caveat, HashSetDenyList,
    HolderId, PublicKey, RequiredCaveats,
};
use ardur_cost_gate::CostEnvelope;
use ardur_multi_agent::{
    CHAT_SUBMIT_TOOL, InMemoryMultiAgentRuntime, MultiAgentError, MultiAgentRuntime,
    SubAgentRequest, SubAgentSpec, TerminationReason,
};
use ardur_runtime::{ChatMessage, ReceiptId, RuntimeError, SessionId};

/// The capability the parent grants and the child drops.
const MEMORY_WRITE_TOOL: &str = "memory.write";

/// Parent/child expiry — far enough out that wall-clock never bears on the
/// real-wire `ask`s (matches the multi-agent suite's `EXPIRY_UNIX`, ~2096).
const PARENT_EXPIRY: u64 = 4_000_000_000;
/// The grandchild's tightened expiry (~2065) — strictly inside the parent's
/// window, yet still ahead of any real wall-clock the suite runs at.
const GRANDCHILD_EXPIRY: u64 = 3_000_000_000;
/// A verification "now" before every expiry (~2023) — every generation is live.
const EARLY_NOW: u64 = 1_700_000_000;
/// A verification "now" *past* the grandchild's expiry but still inside the
/// parent/child window (~2080) — the instant that separates the grandchild's
/// narrowed authority from its ancestors' broader authority.
const BETWEEN_NOW: u64 = 3_500_000_000;
/// A coarse spend ceiling well above the nominal `cost = 1` a chat turn checks.
const BIG_BUDGET: u64 = 1_000_000;

// ---------------------------------------------------------------------------
// Scenario A — the full parent → child → grandchild chain through the real wire.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parent_child_grandchild_chain_narrows_each_generation() {
    let issuer = dev_cap_issuer();
    let root = issuer.public_key();

    // --- 1. Parent holds a root cap with BOTH chat.submit and memory.write. ---
    let parent_token = issuer
        .issue(
            HolderId("spiffe://ardur/agent/root".to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix: PARENT_EXPIRY,
                budget_remaining: BIG_BUDGET,
                tool_allowlist: vec![CHAT_SUBMIT_TOOL.to_string(), MEMORY_WRITE_TOOL.to_string()],
            },
        )
        .expect("issue parent root cap");

    // Sanity: the parent genuinely authorizes both tools at the verifier.
    assert!(
        verify(&parent_token, &root, &req(EARLY_NOW, CHAT_SUBMIT_TOOL, 1)).is_ok(),
        "parent must authorize chat.submit"
    );
    assert!(
        verify(&parent_token, &root, &req(EARLY_NOW, MEMORY_WRITE_TOOL, 1)).is_ok(),
        "parent must authorize memory.write"
    );

    // The parent runtime carries the parent's authority; its sub-agents narrow
    // from it. The verifying wire authorizes the attenuated token on every turn.
    let parent_anchor = ReceiptId::new();
    let parent_runtime =
        InMemoryMultiAgentRuntime::verifying(AUDIENCE, parent_token.clone(), root, parent_anchor);

    // --- 2. Spawn child by attenuating to chat.submit only (drop memory.write). ---
    let child_handle = parent_runtime
        .spawn(spec(
            "child",
            vec![AttenuationRule::RestrictTools(vec![
                CHAT_SUBMIT_TOOL.to_string(),
            ])],
            10_000,
        ))
        .await
        .expect("spawn child");

    let child_token = parent_runtime
        .attenuated_token(&child_handle.agent_id)
        .expect("child attenuated token");
    // The narrowing block shows up as an extra revocation id beyond the parent's.
    assert!(
        child_token.revocation_ids().len() > parent_token.revocation_ids().len(),
        "child cap must carry the parent's blocks plus its narrowing block"
    );

    // --- 3a. Child runs a real, authorized chat.submit through the wire. ---
    let child_resp = parent_runtime
        .ask(&child_handle, ask("child: do the chat work", 50))
        .await
        .expect("child's chat.submit is authorized");
    assert_eq!(child_resp.message.content, "child: do the chat work");
    assert_eq!(parent_runtime.cents_used(&child_handle.agent_id), Some(50));

    // --- 3b. The child's dropped memory.write is denied at the substrate the
    //     wire delegates to. `ask` only ever exercises chat.submit, so the
    //     dropped-tool denial is proven one layer down — at the §11.14 verifier
    //     that `CapVerifyingRuntime` authorizes against. ---
    assert!(
        matches!(
            verify(&child_token, &root, &req(EARLY_NOW, MEMORY_WRITE_TOOL, 1)),
            Err(CapTokenError::ToolNotAllowed)
        ),
        "child must NOT be able to use the memory.write its parent dropped"
    );
    // chat.submit, however, still verifies for the child.
    assert!(
        verify(&child_token, &root, &req(EARLY_NOW, CHAT_SUBMIT_TOOL, 1)).is_ok(),
        "child retains chat.submit"
    );

    // A sub-agent narrowed *away* from chat.submit is denied at the runtime ask
    // layer itself (the §5.1 real-wire denial type) — the runtime-level mirror
    // of the cap-layer denial above.
    let no_chat = parent_runtime
        .spawn(spec(
            "child-no-chat",
            vec![AttenuationRule::RestrictTools(vec![
                MEMORY_WRITE_TOOL.to_string(),
            ])],
            10_000,
        ))
        .await
        .expect("spawn child-no-chat");
    let denied = parent_runtime
        .ask(&no_chat, ask("try to chat without the grant", 10))
        .await
        .expect_err("a sub-agent without chat.submit cannot run a chat turn");
    match denied {
        MultiAgentError::Runtime(RuntimeError::Internal(inner)) => assert!(
            inner
                .to_string()
                .contains("tool not in cap-token allowlist"),
            "expected a tool-allowlist denial, got: {inner}"
        ),
        other => panic!("expected a runtime tool denial, got {other:?}"),
    }
    assert_eq!(parent_runtime.cents_used(&no_chat.agent_id), Some(0));

    // --- 4. The child becomes a parent: a fresh runtime seeded with the CHILD's
    //     attenuated token. Grandchildren narrow from there, so they inherit the
    //     dropped memory.write automatically. ---
    let child_anchor = ReceiptId::new();
    let child_runtime =
        InMemoryMultiAgentRuntime::verifying(AUDIENCE, child_token.clone(), root, child_anchor);

    // Spawn grandchild by attenuating the child's cap with a tighter expiry.
    let grand_handle = child_runtime
        .spawn(spec(
            "grandchild",
            vec![AttenuationRule::EarlierExpiry(GRANDCHILD_EXPIRY)],
            10_000,
        ))
        .await
        .expect("spawn grandchild");

    let grand_token = child_runtime
        .attenuated_token(&grand_handle.agent_id)
        .expect("grandchild attenuated token");
    assert!(
        grand_token.revocation_ids().len() > child_token.revocation_ids().len(),
        "grandchild cap must carry the child's blocks plus its narrowing block"
    );

    // --- 5. Grandchild runs its own authorized chat.submit through the wire
    //     (inside its tighter window — wall-clock now is well before 2065). ---
    let grand_resp = child_runtime
        .ask(&grand_handle, ask("grandchild: do the chat work", 30))
        .await
        .expect("grandchild's chat.submit is authorized inside its window");
    assert_eq!(grand_resp.message.content, "grandchild: do the chat work");

    // --- 6. The grandchild operates inside its narrowed window, but cannot
    //     reach back to the broader window its ancestors still hold. ---
    // Inside the grandchild's window: authorized.
    assert!(
        verify(&grand_token, &root, &req(EARLY_NOW, CHAT_SUBMIT_TOOL, 1)).is_ok(),
        "grandchild is authorized inside its own (tighter) window"
    );
    // Past the grandchild's expiry but still inside the ancestors' window:
    // the grandchild is Expired...
    assert!(
        matches!(
            verify(&grand_token, &root, &req(BETWEEN_NOW, CHAT_SUBMIT_TOOL, 1)),
            Err(CapTokenError::Expired)
        ),
        "grandchild must be denied once past its own expiry"
    );
    // ...while its ancestors are still live at that very instant — proof the
    // grandchild was genuinely narrowed, not that the whole chain expired.
    assert!(
        verify(&child_token, &root, &req(BETWEEN_NOW, CHAT_SUBMIT_TOOL, 1)).is_ok(),
        "the child (ancestor) is still live in the broader window"
    );
    assert!(
        verify(&parent_token, &root, &req(BETWEEN_NOW, CHAT_SUBMIT_TOOL, 1)).is_ok(),
        "the parent (root) is still live in the broader window"
    );

    // The grandchild also inherited the dropped memory.write — it was never
    // re-granted on the way down.
    assert!(
        matches!(
            verify(&grand_token, &root, &req(EARLY_NOW, MEMORY_WRITE_TOOL, 1)),
            Err(CapTokenError::ToolNotAllowed)
        ),
        "grandchild inherits the child's dropped memory.write"
    );

    // --- 7. Tear the chain down; each receipt links its generation's anchor. ---
    let grand_receipt = child_runtime
        .terminate(grand_handle, TerminationReason::Completed)
        .await
        .expect("terminate grandchild");
    assert_eq!(grand_receipt.parent_receipt_id, child_anchor);

    let child_receipt = parent_runtime
        .terminate(child_handle, TerminationReason::Completed)
        .await
        .expect("terminate child");
    assert_eq!(child_receipt.parent_receipt_id, parent_anchor);
    assert_eq!(child_receipt.total_cost.cents, 50);
}

// ---------------------------------------------------------------------------
// Scenario B — escalation is structurally impossible, not a runtime error.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grandchild_cannot_re_grant_a_dropped_capability() {
    let issuer = dev_cap_issuer();
    let root = issuer.public_key();
    let attenuator = BiscuitCapTokenAttenuator;

    // Parent: chat.submit + memory.write.
    let parent_token = issuer
        .issue(
            HolderId("spiffe://ardur/agent/root".to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix: PARENT_EXPIRY,
                budget_remaining: BIG_BUDGET,
                tool_allowlist: vec![CHAT_SUBMIT_TOOL.to_string(), MEMORY_WRITE_TOOL.to_string()],
            },
        )
        .expect("issue parent");

    // Child: drop memory.write. Grandchild: tighten expiry. (Same chain as A,
    // derived directly through the attenuator the runtime uses internally.)
    let child_token = attenuator
        .attenuate(
            &parent_token,
            Caveat::new(AttenuationRule::RestrictTools(vec![
                CHAT_SUBMIT_TOOL.to_string(),
            ])),
        )
        .expect("attenuate child");
    let grand_token = attenuator
        .attenuate(
            &child_token,
            Caveat::new(AttenuationRule::EarlierExpiry(GRANDCHILD_EXPIRY)),
        )
        .expect("attenuate grandchild");

    // The grandchild attempts to ESCALATE: re-list memory.write in a tool
    // restriction, trying to claw back what the child dropped.
    //
    // Finding (documented in the module header): this attempt is *not* rejected
    // by `attenuate` — appending a check never inspects the parent's allowlist,
    // so the call returns Ok. Escalation is instead defeated structurally.
    let escalated = attenuator
        .attenuate(
            &grand_token,
            Caveat::new(AttenuationRule::RestrictTools(vec![
                CHAT_SUBMIT_TOOL.to_string(),
                MEMORY_WRITE_TOOL.to_string(),
            ])),
        )
        .expect("attenuate returns Ok — re-listing a tool is not blocked at the API");

    // ...yet memory.write STILL does not verify: the ancestor block that pinned
    // tools to {chat.submit} continues to reject it. Check-intersection means
    // the re-listing is inert — the capability stays dropped.
    assert!(
        matches!(
            verify(&escalated, &root, &req(EARLY_NOW, MEMORY_WRITE_TOOL, 1)),
            Err(CapTokenError::ToolNotAllowed)
        ),
        "re-listing a dropped tool cannot restore it — intersection wins"
    );

    // And the escalation did not break the capability it legitimately retained:
    // chat.submit still verifies inside the grandchild's window.
    assert!(
        verify(&escalated, &root, &req(EARLY_NOW, CHAT_SUBMIT_TOOL, 1)).is_ok(),
        "the legitimately-held chat.submit survives the (inert) escalation attempt"
    );

    // The other conceivable escalations cannot even be expressed: there is no
    // AttenuationRule variant that widens any axis. We exhaustively match the
    // enum here so that if a future widening variant is ever added, this test
    // fails to compile — a tripwire guarding the append-only invariant.
    let rule = AttenuationRule::RestrictTools(vec![CHAT_SUBMIT_TOOL.to_string()]);
    match rule {
        AttenuationRule::RestrictAudience(_)
        | AttenuationRule::EarlierExpiry(_)
        | AttenuationRule::ReduceBudget(_)
        | AttenuationRule::RestrictTools(_) => {
            // Every variant narrows. Widening is unrepresentable by construction.
        }
    }
}

// ---------------------------------------------------------------------------
// Local helpers (kept inline; this scenario is the only multi-agent E2E).
// ---------------------------------------------------------------------------

/// Verify `token` against `root` for a single request, using a fresh
/// empty-deny-list verifier — the same verification `CapVerifyingRuntime`
/// performs on every turn, but with an explicit `now`/`tool` so the time and
/// tool axes are deterministic.
fn verify(
    token: &CapToken,
    root: &PublicKey,
    required: &RequiredCaveats,
) -> Result<ardur_cap_token::VerifiedClaims, CapTokenError> {
    BiscuitCapTokenVerifier::new(HashSetDenyList::new()).verify(token, root, required)
}

/// A request for `tool` at `now_unix`, always under [`AUDIENCE`].
fn req(now_unix: u64, tool: &str, cost: u64) -> RequiredCaveats {
    RequiredCaveats {
        now_unix,
        audience: AUDIENCE.to_string(),
        tool: tool.to_string(),
        cost,
    }
}

/// A spawn spec under a fresh parent session with the given attenuation + budget.
fn spec(agent_id: &str, attenuation: Vec<AttenuationRule>, cents_max: u32) -> SubAgentSpec {
    SubAgentSpec {
        agent_id: agent_id.into(),
        goal: format!("delegated work for {agent_id}"),
        cap_token_attenuation: attenuation,
        cost_envelope: CostEnvelope {
            cents_max,
            ..CostEnvelope::default()
        },
        parent_session_id: SessionId::new(),
    }
}

/// A user-message ask reserving `max_cost_cents` against the sub-agent envelope.
fn ask(text: &str, max_cost_cents: u32) -> SubAgentRequest {
    SubAgentRequest {
        message: ChatMessage::user(text),
        max_cost_cents,
    }
}
