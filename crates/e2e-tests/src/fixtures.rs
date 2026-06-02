//! Shared fixtures for the §2.E end-to-end scenarios.
//!
//! Everything here is deterministic: the same root key, the same receipt key,
//! and the same permissive policy set on every run, so a scenario's only source
//! of variance is the code under test. The substrate's own constructors are
//! re-exported through thin helpers so a scenario reads as a sequence of
//! intent (`dev_cap_root`, `stub_provider`, `permissive_policies`) rather than
//! a wall of builder boilerplate.

use std::sync::Arc;

use ardur_cap_token::{
    BiscuitCapTokenIssuer, CapScope, CapTokenIssuer, HolderId as CapHolderId, KeyPair,
};
use ardur_cedar_policy::{CedarPolicyBundle, PolicyBundle, PolicySource};
use ardur_cost_gate::{Clock, CostTuple as GateCostTuple, HolderId as GateHolderId, ManualClock};
use ardur_fused_runtime::FusedRuntimeBuilder;
use ardur_provider_runtime::{AnthropicProvider, ModelId, Provider};
use ardur_receipt::Es256SigningKey;
use biscuit_auth::PrivateKey;
use tempfile::TempDir;

/// The model id the stub provider and scenarios complete against.
pub const TEST_MODEL: &str = "claude-opus-4-8";

/// The SPIFFE-style holder the fused-runtime scenarios run as.
pub const TEST_HOLDER: &str = "spiffe://ardur/user/e2e";
/// The audience the fused-runtime cap-tokens are scoped to.
pub const AUDIENCE: &str = "ardur-cli";
/// The tool/capability the fused-runtime turns exercise.
pub const TOOL: &str = "chat.submit";
/// A fixed "now" (seconds) for the cap-token caveats.
pub const NOW_UNIX: u64 = 1_750_000_000;
/// The same instant in milliseconds, for the cost-gate clock + timestamps.
pub const NOW_MS: u64 = 1_750_000_000_000;

/// A fixed 32-byte Ed25519 seed for the cap-token root key. A constant seed
/// makes [`dev_cap_root`] return the same root key — and therefore the same
/// root public key a verifier checks against — on every run.
const CAP_ROOT_SEED: [u8; 32] = [
    0x2e, 0x45, 0x31, 0xa7, 0x0c, 0x9b, 0x6d, 0x14, 0xf3, 0x88, 0x21, 0x5c, 0xbe, 0x47, 0x90, 0xd2,
    0x6a, 0x1f, 0x33, 0x70, 0xc8, 0x05, 0x9e, 0x42, 0xab, 0x77, 0x18, 0xe9, 0x54, 0x3c, 0x6b, 0x0d,
];

/// A fixed P-256 PKCS#8 private key (PEM) for the receipt signer. This is a
/// throwaway test key generated once and embedded so [`dev_receipt_key`] is
/// deterministic; it never signs anything outside the test suite.
// TODO §E1: the original plan said "initialize a ReceiptChain with the cap-token
// keypair". The receipt crate signs with a *P-256* `Es256SigningKey`, while
// cap-token is *Ed25519* (biscuit) — distinct curves, distinct key custody — and
// `ReceiptChain::append` takes no key at all. So the receipt key is its own
// fixture rather than a reuse of `dev_cap_root`.
const RECEIPT_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg1H+4Ujdw06BiHyA2\n\
qKq8O0Lk7Or+Dy4ARqWo2dkZ6YChRANCAASNP0FpRaZyEmB9cYqXqbPdjc+xIFjQ\n\
oMz32N5zmQUNlJpyLV/9oTufQKsNFzSjxvrwjMZiTc1twZT9s+aGKBnA\n\
-----END PRIVATE KEY-----\n";

/// A fresh temp directory to root journal (and any other on-disk) state for one
/// scenario. The returned [`TempDir`] deletes the tree when dropped, so hold it
/// for the lifetime of the scenario.
#[must_use]
pub fn temp_session_root() -> TempDir {
    tempfile::tempdir().expect("a temp session root can be created")
}

/// The deterministic Ed25519 cap-token *root* key pair.
///
/// Built fresh from [`CAP_ROOT_SEED`] on each call (the seed, not the handle, is
/// what is shared), so two calls yield byte-identical key material.
// TODO §E1: the plan named this `CapTokenKeypair`; the crate's type is biscuit's
// `KeyPair` (re-exported by `ardur-cap-token`).
#[must_use]
pub fn dev_cap_root() -> KeyPair {
    let private = PrivateKey::from_bytes(&CAP_ROOT_SEED)
        .expect("CAP_ROOT_SEED is a valid 32-byte Ed25519 private key");
    KeyPair::from(&private)
}

/// An issuer over the deterministic [`dev_cap_root`] key — the convenience the
/// scenarios actually mint root tokens through.
#[must_use]
pub fn dev_cap_issuer() -> BiscuitCapTokenIssuer {
    BiscuitCapTokenIssuer::new(dev_cap_root())
}

/// The deterministic P-256 signing key for receipts, loaded from the embedded
/// [`RECEIPT_KEY_PEM`].
#[must_use]
pub fn dev_receipt_key() -> Es256SigningKey {
    Es256SigningKey::from_pkcs8_pem(RECEIPT_KEY_PEM)
        .expect("the embedded test receipt key is a valid P-256 PKCS#8 PEM")
}

/// The deterministic Anthropic *stub* provider: no network, a fixed
/// `"[anthropic stub]"` completion.
#[must_use]
pub fn stub_provider() -> AnthropicProvider {
    AnthropicProvider::stub(ModelId::new(TEST_MODEL))
}

/// A permissive Cedar bundle — one unconditional `permit` — for happy-path
/// scenarios that need the authorization seam to say `Allow`.
// TODO §E1: the plan named this `CedarPolicySet` returning a `CedarPolicyEngine`;
// the crate's type is `CedarPolicyBundle` (a `PolicyBundle`).
#[must_use]
pub fn permissive_policies() -> CedarPolicyBundle {
    CedarPolicyBundle::load(PolicySource::Embedded(
        "permit(principal, action, resource);".to_string(),
    ))
    .expect("the permissive policy compiles")
}

/// A deterministic manual clock pinned at [`NOW_MS`], so the cost-gate's
/// reservation expiry and the cap-token `now` caveat are race-free.
#[must_use]
pub fn dev_clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(NOW_MS))
}

/// Mint a cap-token (base64) for [`TEST_HOLDER`], scoped to [`AUDIENCE`] /
/// [`TOOL`], with the given expiry (seconds) and coarse budget.
#[must_use]
pub fn dev_cap_token(expires_unix: u64, budget_remaining: u64) -> String {
    dev_cap_issuer()
        .issue(
            CapHolderId(TEST_HOLDER.to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix,
                budget_remaining,
                tool_allowlist: vec![TOOL.to_string()],
            },
        )
        .expect("the cap-token issues")
        .to_base64()
        .expect("the cap-token serializes")
}

/// A valid (well-funded, unexpired) cap-token for the fused-runtime scenarios.
#[must_use]
pub fn dev_valid_cap_token() -> String {
    dev_cap_token(NOW_UNIX + 3_600, 1_000_000)
}

/// A [`FusedRuntimeBuilder`] pre-wired with the deterministic cap-token root, the
/// permissive policy, the deterministic receipt key, the manual clock, the
/// audience/tool, and a generous budget for [`TEST_HOLDER`]. Scenarios tweak the
/// budget, envelope, memory, journal, and receipt-log as needed.
#[must_use]
pub fn fused_builder(provider: Arc<dyn Provider>) -> FusedRuntimeBuilder {
    fused_builder_with_policies(provider, permissive_policies())
}

/// Like [`fused_builder`], but with a caller-supplied Cedar bundle in the
/// authorization seam — the one knob scenario §2.E7 varies while everything
/// else (deterministic cap-token root, permissive cap-token, stub provider,
/// manual clock, generous budget) stays fixed, so the policy decision is the
/// sole cause of a turn's outcome.
#[must_use]
pub fn fused_builder_with_policies(
    provider: Arc<dyn Provider>,
    policies: CedarPolicyBundle,
) -> FusedRuntimeBuilder {
    FusedRuntimeBuilder::new(
        dev_cap_issuer().public_key(),
        policies,
        provider,
        dev_receipt_key(),
        ModelId::new(TEST_MODEL),
    )
    .audience(AUDIENCE)
    .tool(TOOL)
    .clock(dev_clock())
    .provision_budget(
        GateHolderId(TEST_HOLDER.to_string()),
        GateCostTuple {
            tokens_in: 1_000_000_000,
            tokens_out: 1_000_000_000,
            cents: 1_000_000,
            wall_ms: 1_000_000_000,
            attention_score: 1_000_000_000,
        },
    )
}

/// The gate holder id for [`TEST_HOLDER`].
#[must_use]
pub fn gate_holder() -> GateHolderId {
    GateHolderId(TEST_HOLDER.to_string())
}
