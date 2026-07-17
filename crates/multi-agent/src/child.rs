//! The real-wire child runtime: a [`ChatRuntime`] that *enforces* a sub-agent's
//! attenuated cap-token before it runs a turn.
//!
//! # Why this exists (§5.1 — "real wire")
//!
//! §5.0 landed the attenuation contracts: [`MultiAgentRuntime::spawn`] narrows
//! the parent token by each [`AttenuationRule`] and stashes the result, and the
//! [`attenuated_token`] accessor lets an auditor inspect the narrowed authority.
//! But the §5.0 default child surface — `ardur_runtime::InMemoryRuntime` — is an
//! echo that only checks the cap-token *string is non-empty*. It never re-binds
//! the token to the issuer root nor authorizes it against the turn, so the
//! attenuation has no teeth at request time: a sub-agent whose `chat.submit`
//! capability was attenuated away could still echo a turn.
//!
//! [`CapVerifyingRuntime`] closes that gap. It wraps an inner [`ChatRuntime`]
//! and, on every [`submit`](ChatRuntime::submit), re-binds the presented
//! cap-token to the issuer root and authorizes it for the [`CHAT_SUBMIT_TOOL`]
//! capability under the runtime's audience at the current wall-clock time. Only
//! if that authorization passes does the turn reach the inner runtime. So the
//! attenuated cap now genuinely *flows through the substrate*: narrowing the
//! tool allowlist, the audience, or the expiry actually gates the turn.
//!
//! # What it enforces, and what it leaves to the envelope
//!
//! The cap-token carries four axes (audience, expiry, tool allowlist, budget).
//! This runtime enforces the first three — the authority axes that do not depend
//! on a per-turn cost. The fourth (spend) is the job of the per-sub-agent
//! [`CostEnvelope`] meter in [`InMemoryMultiAgentRuntime`], which reserves a
//! turn's declared cents *before* the child runs; so the cap-token's budget
//! caveat is exercised here only nominally (cost `1`), and the real ceiling is
//! the envelope. Keeping the two concerns separate means a `ReduceBudget`
//! attenuation and a tight envelope can't be confused for one another.
//!
//! [`MultiAgentRuntime::spawn`]: crate::MultiAgentRuntime::spawn
//! [`attenuated_token`]: crate::InMemoryMultiAgentRuntime::attenuated_token
//! [`InMemoryMultiAgentRuntime`]: crate::InMemoryMultiAgentRuntime
//! [`AttenuationRule`]: ardur_cap_token::AttenuationRule
//! [`CostEnvelope`]: ardur_cost_gate::CostEnvelope

use std::time::{SystemTime, UNIX_EPOCH};

use ardur_cap_token::{
    BiscuitCapTokenVerifier, CapToken, CapTokenError, CapTokenVerifier, HashSetDenyList, PublicKey,
    RequiredCaveats,
};
use ardur_runtime::{ChatRuntime, RuntimeError, SubmitRequest, SubmitResult};

/// The capability a chat turn exercises. A sub-agent must hold `chat.submit` in
/// its (attenuated) tool allowlist for [`CapVerifyingRuntime`] to admit a turn;
/// a sub-agent narrowed to a disjoint tool set is denied here.
pub const CHAT_SUBMIT_TOOL: &str = "chat.submit";

/// A [`ChatRuntime`] that authorizes the presented (attenuated) cap-token
/// against the issuer root before delegating the turn to an inner runtime.
///
/// This is the §5.1 real wire: unlike the echo `ardur_runtime::InMemoryRuntime`,
/// which trusts any non-empty token, this runtime re-binds the token to `root`
/// and authorizes it for [`CHAT_SUBMIT_TOOL`] under `audience` at the current
/// time. A turn whose sub-agent token was attenuated to drop `chat.submit`,
/// restrict the audience, or bring the expiry into the past is rejected before
/// the inner runtime ever sees it.
pub struct CapVerifyingRuntime<R: ChatRuntime> {
    /// The runtime that actually produces the response once authority is proven.
    inner: R,
    /// The issuer root the presented token's block signatures must verify
    /// against.
    root: PublicKey,
    /// The audience every authorized turn is checked against (must match the
    /// audience the parent token — and so every narrowing of it — was issued
    /// for).
    audience: String,
    /// A stateless verifier over an empty deny list. Revocation-by-deny-list is
    /// a §11.14 verifier concern; Phase 1 carries no revocations here.
    // TODO §5.0 Phase 2: thread a shared, mutable deny list so a parent can
    // revoke a live sub-agent's authority mid-flight (pairs with the §11.17
    // lifecycle-hook `on_revoke` surface once it lands).
    verifier: BiscuitCapTokenVerifier<HashSetDenyList>,
}

impl<R: ChatRuntime> CapVerifyingRuntime<R> {
    /// Wrap `inner` with cap-token enforcement against issuer `root` for
    /// `audience`.
    pub fn new(inner: R, root: PublicKey, audience: impl Into<String>) -> Self {
        Self {
            inner,
            root,
            audience: audience.into(),
            verifier: BiscuitCapTokenVerifier::new(HashSetDenyList::new()),
        }
    }

    /// The issuer root this runtime authorizes presented tokens against.
    pub fn root_public_key(&self) -> &PublicKey {
        &self.root
    }
}

#[async_trait::async_trait]
impl<R: ChatRuntime> ChatRuntime for CapVerifyingRuntime<R> {
    async fn submit(&self, req: SubmitRequest) -> Result<SubmitResult, RuntimeError> {
        // An empty token string is "missing", not "denied" — match the echo
        // runtime's framing so the two surfaces agree on that boundary case.
        if req.cap_token.0.is_empty() {
            return Err(RuntimeError::CapTokenMissing);
        }

        // Re-bind the presented (attenuated) token to the issuer root, then
        // authorize it for a chat turn: the audience the runtime serves, the
        // `chat.submit` capability, at the current wall-clock second. This is
        // where attenuation gains teeth — every narrowing the parent applied is
        // intersected into the token's checks and re-evaluated here.
        let token = CapToken::from_base64(&req.cap_token.0, &self.root).map_err(map_cap_error)?;
        let required = RequiredCaveats {
            now_unix: now_secs(),
            audience: self.audience.clone(),
            tool: CHAT_SUBMIT_TOOL.to_string(),
            // The lifetime spend ceiling is the sub-agent's `CostEnvelope`, not
            // this caveat — a chat turn costs a nominal `1` for authority
            // purposes so the budget axis never spuriously rejects an
            // in-envelope turn.
            cost: 1,
        };
        self.verifier
            .verify(&token, &self.root, &required)
            .map_err(map_cap_error)?;

        // Authority proven — hand the turn to the inner runtime.
        self.inner.submit(req).await
    }
}

/// Wall-clock now, in whole seconds since the Unix epoch — the time axis the
/// cap-token's expiry caveat is evaluated against.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Project a [`CapTokenError`] onto the §1.0 [`RuntimeError`] surface. §1.0 has
/// no dedicated "unauthorized" variant, so an authority denial (wrong tool,
/// audience, or budget) degrades to [`RuntimeError::Internal`] carrying the
/// precise cap-token reason; expiry maps to the existing
/// [`RuntimeError::CapTokenExpired`], and an undecodable/forged token to
/// [`RuntimeError::CapTokenMissing`] (the "unresolved token" framing).
// TODO §1.0 Phase 2: add a `RuntimeError::Unauthorized(CapTokenError)` variant
// so a cap denial is typed rather than folded into `Internal` (out of scope for
// §5.1, which may only touch `crates/multi-agent`).
fn map_cap_error(err: CapTokenError) -> RuntimeError {
    match err {
        CapTokenError::Expired => RuntimeError::CapTokenExpired,
        CapTokenError::Malformed(_) | CapTokenError::SignatureInvalid => {
            RuntimeError::CapTokenMissing
        }
        denied => RuntimeError::Internal(anyhow::anyhow!("cap-token denied at submit: {denied}")),
    }
}
