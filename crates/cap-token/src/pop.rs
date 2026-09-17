//! Proof-of-possession binding for cap-tokens (#363, AAT invariant I6).
//!
//! # The problem
//!
//! A cap-token is a pure bearer credential: verification checks the signature,
//! the deny-list and the time/audience/tool/cost caveats, and nothing binds the
//! token to *who is presenting it*. Anyone who captures one becomes its subject
//! for every authorization decision until it expires. A captured request can be
//! replayed verbatim.
//!
//! # What this adds
//!
//! A `cnf` (confirmation) claim naming the holder's public key by thumbprint,
//! and a per-request proof the holder signs with the matching private key. The
//! verifier recomputes the thumbprint from the presented key, checks it equals
//! the bound one, then checks the signature over a canonical binding string.
//!
//! Stealing the token is then not enough: the thief also needs the private key,
//! which never travels with the token.
//!
//! # Replay
//!
//! The proof covers a caller-supplied nonce and an issued-at timestamp, so the
//! same signature cannot be replayed against a different request. Nonce reuse is
//! rejected within the acceptance window by [`ReplayCache`]; a proof outside the
//! window is rejected on age alone, which bounds how much state the cache needs
//! to hold.
//!
//! # What this deliberately does NOT do
//!
//! It does not weaken anything for tokens without a `cnf` claim. Legacy tokens
//! keep their current (bearer) semantics, and a caller that wants PoP enforced
//! sets [`PopRequirement::Required`] — at which point a token with no binding is
//! **rejected**, not waved through. That choice is the caller's to make
//! explicitly, because silently accepting unbound tokens under a "PoP enabled"
//! configuration is exactly the kind of fail-open this crate must not have.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::error::CapTokenError;

/// Maximum age of a PoP proof, in seconds.
///
/// A proof older than this is rejected regardless of its signature. This is what
/// bounds the replay cache: nonces older than the window can be evicted, because
/// a proof carrying them would fail the age check anyway.
pub const DEFAULT_PROOF_MAX_AGE_SECS: u64 = 300;

/// Maximum clock skew tolerated on a proof's `issued_at`, in seconds.
///
/// Without this a holder whose clock runs marginally fast produces proofs that
/// look like they come from the future and are rejected. Kept deliberately small
/// — a wide skew window extends the effective replay window by the same amount.
pub const DEFAULT_MAX_CLOCK_SKEW_SECS: u64 = 30;

/// Whether the verifier insists on proof-of-possession.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopRequirement {
    /// Accept tokens without a `cnf` binding (legacy bearer semantics).
    ///
    /// A token that *does* carry a binding is still checked: opting out of
    /// requiring PoP must never mean ignoring a proof that was supplied.
    Optional,
    /// Reject any token that does not carry a `cnf` binding AND a valid proof.
    Required,
}

/// A holder's public key thumbprint, as bound into a token's `cnf` claim.
///
/// Stored as `sha-256:` + 64 lowercase hex over the raw Ed25519 public key.
/// The thumbprint — not the key — travels in the token, so a token leak does
/// not leak anything usable for impersonation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct KeyThumbprint(String);

impl KeyThumbprint {
    /// Compute the thumbprint of an Ed25519 verifying key.
    #[must_use]
    pub fn of(key: &VerifyingKey) -> Self {
        let digest = Sha256::digest(key.as_bytes());
        let hex = digest.iter().fold(String::with_capacity(64), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        });
        Self(format!("sha-256:{hex}"))
    }

    /// The thumbprint string, `sha-256:<64 hex>`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse a thumbprint, rejecting anything that is not the exact shape.
    ///
    /// # Errors
    ///
    /// [`CapTokenError::Malformed`] when the prefix, length or alphabet is
    /// wrong. Strict parsing matters: a permissive parser that accepted an
    /// empty or truncated thumbprint would make binding comparisons trivially
    /// satisfiable.
    pub fn parse(s: &str) -> Result<Self, CapTokenError> {
        let hex = s.strip_prefix("sha-256:").ok_or_else(|| {
            CapTokenError::Malformed("thumbprint must carry the sha-256: prefix".into())
        })?;
        if hex.len() != 64 {
            return Err(CapTokenError::Malformed(format!(
                "thumbprint must be 64 hex characters, got {}",
                hex.len()
            )));
        }
        if !hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(CapTokenError::Malformed(
                "thumbprint must be lowercase hex".into(),
            ));
        }
        Ok(Self(s.to_string()))
    }
}

/// The confirmation claim bound into a token at issuance.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Confirmation {
    /// The holder key's thumbprint (`jkt`, per RFC 9449's naming).
    pub jkt: KeyThumbprint,
}

/// What a proof commits to.
///
/// Every field that distinguishes one request from another belongs here. A field
/// omitted from the binding is a field an attacker may vary freely while
/// replaying a captured signature — which is why the tool and cost are included
/// and not just the token id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestBinding {
    /// The token this proof accompanies.
    pub token_id: String,
    /// The tool the request will invoke.
    pub tool: String,
    /// The budget units the request will consume.
    pub cost: u64,
    /// The audience presenting the token.
    pub audience: String,
    /// A caller-chosen value that must not repeat within the acceptance window.
    pub nonce: String,
    /// When the proof was created, Unix seconds.
    pub issued_at: u64,
}

impl RequestBinding {
    /// The exact bytes a holder signs.
    ///
    /// Length-prefixed rather than delimiter-joined: with a plain separator a
    /// caller could move characters between adjacent fields (`tool="a", nonce="b:c"`
    /// vs `tool="a:b", nonce="c"`) and produce the same signing input, so one
    /// signature would authorize two different requests.
    #[must_use]
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"ardur-cap-pop-v1");
        for field in [
            self.token_id.as_str(),
            self.tool.as_str(),
            self.audience.as_str(),
            self.nonce.as_str(),
        ] {
            out.extend_from_slice(&(field.len() as u64).to_be_bytes());
            out.extend_from_slice(field.as_bytes());
        }
        out.extend_from_slice(&self.cost.to_be_bytes());
        out.extend_from_slice(&self.issued_at.to_be_bytes());
        out
    }
}

/// A holder's proof that it possesses the bound private key.
#[derive(Clone, Debug)]
pub struct PopProof {
    /// The holder's public key, presented alongside the proof.
    pub public_key: VerifyingKey,
    /// Ed25519 signature over [`RequestBinding::signing_bytes`].
    pub signature: Signature,
    /// The binding the signature covers.
    pub binding: RequestBinding,
}

impl PopProof {
    /// Sign a binding with a holder's key.
    #[must_use]
    pub fn create(signing_key: &ed25519_dalek::SigningKey, binding: RequestBinding) -> Self {
        use ed25519_dalek::Signer as _;
        let signature = signing_key.sign(&binding.signing_bytes());
        Self {
            public_key: signing_key.verifying_key(),
            signature,
            binding,
        }
    }

    /// The presented key's thumbprint.
    #[must_use]
    pub fn thumbprint(&self) -> KeyThumbprint {
        KeyThumbprint::of(&self.public_key)
    }

    /// Base64url of the signature, for transport.
    #[must_use]
    pub fn signature_b64(&self) -> String {
        B64URL.encode(self.signature.to_bytes())
    }
}

/// Bounded nonce cache rejecting replays inside the acceptance window.
///
/// Entries older than the window are evicted on insert: a proof carrying such a
/// nonce is rejected on age anyway, so retaining it buys nothing and lets an
/// attacker grow the cache without bound.
#[derive(Debug, Default)]
pub struct ReplayCache {
    seen: BTreeMap<String, u64>,
    max_age_secs: u64,
}

impl ReplayCache {
    /// A cache holding nonces for `max_age_secs`.
    #[must_use]
    pub fn new(max_age_secs: u64) -> Self {
        Self {
            seen: BTreeMap::new(),
            max_age_secs,
        }
    }

    /// Record a nonce, returning `false` when it was already used.
    pub fn record(&mut self, nonce: &str, now_unix: u64) -> bool {
        let cutoff = now_unix.saturating_sub(self.max_age_secs);
        self.seen.retain(|_, seen_at| *seen_at >= cutoff);
        if self.seen.contains_key(nonce) {
            return false;
        }
        self.seen.insert(nonce.to_string(), now_unix);
        true
    }

    /// How many nonces are currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// Verify a proof against a token's binding and the concrete request.
///
/// # Errors
///
/// - [`CapTokenError::PopRequired`] — PoP is required but the token carries no
///   `cnf`, or no proof was presented.
/// - [`CapTokenError::PopKeyMismatch`] — the presented key is not the bound one.
/// - [`CapTokenError::PopInvalid`] — bad signature, stale/future proof, a
///   binding that does not describe this request, or a replayed nonce.
pub fn verify_pop(
    confirmation: Option<&Confirmation>,
    proof: Option<&PopProof>,
    request: &RequestBinding,
    requirement: PopRequirement,
    now_unix: u64,
    replay: &mut ReplayCache,
) -> Result<(), CapTokenError> {
    let (confirmation, proof) = match (confirmation, proof, requirement) {
        // Bound token + proof: always verified, even when PoP is Optional.
        // Ignoring a supplied proof would silently discard the one signal that
        // distinguishes the holder from a thief.
        (Some(c), Some(p), _) => (c, p),

        // A bound token presented with no proof is a failure in BOTH modes: the
        // issuer already decided this token needs possession, and honouring the
        // binding only when the caller opts in would let the mode downgrade it.
        (Some(_), None, _) => {
            return Err(CapTokenError::PopRequired(
                "token carries a cnf binding but no proof was presented".into(),
            ));
        }

        // Unbound token under Required: reject. Accepting it would make the
        // "required" setting a lie.
        (None, _, PopRequirement::Required) => {
            return Err(CapTokenError::PopRequired(
                "proof-of-possession is required but the token carries no cnf binding".into(),
            ));
        }

        // A proof for an unbound token proves nothing — there is no bound key to
        // compare against, so any key would satisfy it.
        (None, Some(_), PopRequirement::Optional) => {
            return Err(CapTokenError::PopInvalid(
                "a proof was presented for a token with no cnf binding".into(),
            ));
        }

        // Legacy bearer token, PoP not required.
        (None, None, PopRequirement::Optional) => return Ok(()),
    };

    // 1. The presented key must be the bound key.
    let presented = proof.thumbprint();
    if presented != confirmation.jkt {
        return Err(CapTokenError::PopKeyMismatch {
            expected: confirmation.jkt.as_str().to_string(),
            presented: presented.as_str().to_string(),
        });
    }

    // 2. The binding must describe THIS request. Checked before the signature so
    //    a valid signature over someone else's request cannot be mistaken for
    //    authorization of this one.
    if proof.binding.token_id != request.token_id
        || proof.binding.tool != request.tool
        || proof.binding.cost != request.cost
        || proof.binding.audience != request.audience
    {
        return Err(CapTokenError::PopInvalid(
            "proof binding does not describe this request".into(),
        ));
    }

    // 3. Freshness. A future-dated proof is rejected beyond the skew allowance;
    //    otherwise a holder could mint long-lived proofs in advance.
    if proof.binding.issued_at > now_unix.saturating_add(DEFAULT_MAX_CLOCK_SKEW_SECS) {
        return Err(CapTokenError::PopInvalid(
            "proof is dated in the future".into(),
        ));
    }
    let age = now_unix.saturating_sub(proof.binding.issued_at);
    if age > DEFAULT_PROOF_MAX_AGE_SECS {
        return Err(CapTokenError::PopInvalid(format!(
            "proof is {age}s old, limit is {DEFAULT_PROOF_MAX_AGE_SECS}s"
        )));
    }

    // 4. The signature itself.
    proof
        .public_key
        .verify(&proof.binding.signing_bytes(), &proof.signature)
        .map_err(|_| CapTokenError::PopInvalid("proof signature does not verify".into()))?;

    // 5. Replay. Recorded LAST so a rejected proof cannot burn a nonce — an
    //    attacker replaying garbage would otherwise lock out the legitimate
    //    holder's next use of that nonce.
    if !replay.record(&proof.binding.nonce, now_unix) {
        return Err(CapTokenError::PopInvalid(
            "proof nonce has already been used".into(),
        ));
    }

    Ok(())
}

/// How long a proof stays acceptable.
#[must_use]
pub fn proof_acceptance_window() -> Duration {
    Duration::from_secs(DEFAULT_PROOF_MAX_AGE_SECS)
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn binding() -> RequestBinding {
        RequestBinding {
            token_id: "01a0adca-0000-4000-8000-00000000000e".into(),
            tool: "file.read".into(),
            cost: 1,
            audience: "cli://localhost".into(),
            nonce: "nonce-1".into(),
            issued_at: 1_789_621_936,
        }
    }

    fn cnf(k: &SigningKey) -> Confirmation {
        Confirmation {
            jkt: KeyThumbprint::of(&k.verifying_key()),
        }
    }

    #[test]
    fn a_valid_proof_from_the_bound_key_is_accepted() {
        let k = key(1);
        let proof = PopProof::create(&k, binding());
        let mut cache = ReplayCache::new(DEFAULT_PROOF_MAX_AGE_SECS);
        assert!(
            verify_pop(
                Some(&cnf(&k)),
                Some(&proof),
                &binding(),
                PopRequirement::Required,
                1_789_621_940,
                &mut cache,
            )
            .is_ok()
        );
    }

    #[test]
    fn a_stolen_token_with_a_different_key_is_rejected() {
        // The whole point: possessing the token is not enough.
        let bound = key(1);
        let thief = key(2);
        let proof = PopProof::create(&thief, binding());
        let mut cache = ReplayCache::new(DEFAULT_PROOF_MAX_AGE_SECS);
        let err = verify_pop(
            Some(&cnf(&bound)),
            Some(&proof),
            &binding(),
            PopRequirement::Required,
            1_789_621_940,
            &mut cache,
        )
        .unwrap_err();
        assert!(
            matches!(err, CapTokenError::PopKeyMismatch { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_bound_token_without_a_proof_is_rejected_even_when_pop_is_optional() {
        // The issuer already decided this token needs possession; the verifier's
        // mode must not be able to downgrade that.
        let k = key(1);
        let mut cache = ReplayCache::new(DEFAULT_PROOF_MAX_AGE_SECS);
        let err = verify_pop(
            Some(&cnf(&k)),
            None,
            &binding(),
            PopRequirement::Optional,
            1_789_621_940,
            &mut cache,
        )
        .unwrap_err();
        assert!(matches!(err, CapTokenError::PopRequired(_)), "{err:?}");
    }

    #[test]
    fn an_unbound_token_is_rejected_when_pop_is_required() {
        let mut cache = ReplayCache::new(DEFAULT_PROOF_MAX_AGE_SECS);
        let err = verify_pop(
            None,
            None,
            &binding(),
            PopRequirement::Required,
            1_789_621_940,
            &mut cache,
        )
        .unwrap_err();
        assert!(matches!(err, CapTokenError::PopRequired(_)), "{err:?}");
    }

    #[test]
    fn a_legacy_bearer_token_still_works_when_pop_is_optional() {
        let mut cache = ReplayCache::new(DEFAULT_PROOF_MAX_AGE_SECS);
        assert!(
            verify_pop(
                None,
                None,
                &binding(),
                PopRequirement::Optional,
                1_789_621_940,
                &mut cache,
            )
            .is_ok()
        );
    }

    #[test]
    fn a_proof_for_an_unbound_token_is_rejected() {
        // There is no bound key to compare against, so accepting it would let
        // ANY key satisfy the check.
        let k = key(1);
        let proof = PopProof::create(&k, binding());
        let mut cache = ReplayCache::new(DEFAULT_PROOF_MAX_AGE_SECS);
        let err = verify_pop(
            None,
            Some(&proof),
            &binding(),
            PopRequirement::Optional,
            1_789_621_940,
            &mut cache,
        )
        .unwrap_err();
        assert!(matches!(err, CapTokenError::PopInvalid(_)), "{err:?}");
    }

    #[test]
    fn a_replayed_proof_is_rejected_the_second_time() {
        let k = key(1);
        let proof = PopProof::create(&k, binding());
        let mut cache = ReplayCache::new(DEFAULT_PROOF_MAX_AGE_SECS);
        assert!(
            verify_pop(
                Some(&cnf(&k)),
                Some(&proof),
                &binding(),
                PopRequirement::Required,
                1_789_621_940,
                &mut cache,
            )
            .is_ok()
        );
        let err = verify_pop(
            Some(&cnf(&k)),
            Some(&proof),
            &binding(),
            PopRequirement::Required,
            1_789_621_941,
            &mut cache,
        )
        .unwrap_err();
        assert!(matches!(err, CapTokenError::PopInvalid(_)), "{err:?}");
    }

    #[test]
    fn a_proof_for_a_different_tool_does_not_authorize_this_request() {
        // Capturing a proof for a cheap read must not authorize a write.
        let k = key(1);
        let mut signed = binding();
        signed.tool = "file.read".into();
        let proof = PopProof::create(&k, signed);

        let mut requested = binding();
        requested.tool = "file.write".into();

        let mut cache = ReplayCache::new(DEFAULT_PROOF_MAX_AGE_SECS);
        let err = verify_pop(
            Some(&cnf(&k)),
            Some(&proof),
            &requested,
            PopRequirement::Required,
            1_789_621_940,
            &mut cache,
        )
        .unwrap_err();
        assert!(matches!(err, CapTokenError::PopInvalid(_)), "{err:?}");
    }

    #[test]
    fn a_proof_for_a_smaller_cost_does_not_authorize_a_larger_spend() {
        let k = key(1);
        let proof = PopProof::create(&k, binding());
        let mut requested = binding();
        requested.cost = 1_000;
        let mut cache = ReplayCache::new(DEFAULT_PROOF_MAX_AGE_SECS);
        let err = verify_pop(
            Some(&cnf(&k)),
            Some(&proof),
            &requested,
            PopRequirement::Required,
            1_789_621_940,
            &mut cache,
        )
        .unwrap_err();
        assert!(matches!(err, CapTokenError::PopInvalid(_)), "{err:?}");
    }

    #[test]
    fn a_stale_proof_is_rejected() {
        let k = key(1);
        let proof = PopProof::create(&k, binding());
        let mut cache = ReplayCache::new(DEFAULT_PROOF_MAX_AGE_SECS);
        let err = verify_pop(
            Some(&cnf(&k)),
            Some(&proof),
            &binding(),
            PopRequirement::Required,
            1_789_621_936 + DEFAULT_PROOF_MAX_AGE_SECS + 1,
            &mut cache,
        )
        .unwrap_err();
        assert!(matches!(err, CapTokenError::PopInvalid(_)), "{err:?}");
    }

    #[test]
    fn a_future_dated_proof_is_rejected_beyond_the_skew_allowance() {
        let k = key(1);
        let mut b = binding();
        b.issued_at = 1_789_621_936 + DEFAULT_MAX_CLOCK_SKEW_SECS + 60;
        let proof = PopProof::create(&k, b);
        let mut cache = ReplayCache::new(DEFAULT_PROOF_MAX_AGE_SECS);
        let err = verify_pop(
            Some(&cnf(&k)),
            Some(&proof),
            &binding(),
            PopRequirement::Required,
            1_789_621_936,
            &mut cache,
        )
        .unwrap_err();
        assert!(matches!(err, CapTokenError::PopInvalid(_)), "{err:?}");
    }

    #[test]
    fn a_tampered_signature_is_rejected() {
        let k = key(1);
        let mut proof = PopProof::create(&k, binding());
        let mut bytes = proof.signature.to_bytes();
        bytes[0] ^= 0xff;
        proof.signature = Signature::from_bytes(&bytes);
        let mut cache = ReplayCache::new(DEFAULT_PROOF_MAX_AGE_SECS);
        let err = verify_pop(
            Some(&cnf(&k)),
            Some(&proof),
            &binding(),
            PopRequirement::Required,
            1_789_621_940,
            &mut cache,
        )
        .unwrap_err();
        assert!(matches!(err, CapTokenError::PopInvalid(_)), "{err:?}");
    }

    #[test]
    fn a_rejected_proof_does_not_burn_the_nonce() {
        // Otherwise an attacker replaying garbage locks out the legitimate
        // holder's next use of that nonce.
        let k = key(1);
        let thief = key(2);
        let bad = PopProof::create(&thief, binding());
        let mut cache = ReplayCache::new(DEFAULT_PROOF_MAX_AGE_SECS);
        let _ = verify_pop(
            Some(&cnf(&k)),
            Some(&bad),
            &binding(),
            PopRequirement::Required,
            1_789_621_940,
            &mut cache,
        );
        assert!(cache.is_empty(), "a failed proof must not record its nonce");

        let good = PopProof::create(&k, binding());
        assert!(
            verify_pop(
                Some(&cnf(&k)),
                Some(&good),
                &binding(),
                PopRequirement::Required,
                1_789_621_940,
                &mut cache,
            )
            .is_ok()
        );
    }

    #[test]
    fn field_boundaries_cannot_be_shifted_between_adjacent_fields() {
        // With delimiter-joined signing input these two would hash identically,
        // so one signature would authorize both requests.
        // The fields must be ADJACENT in the signing order
        // (token_id, tool, audience, nonce) for the shift to be constructible;
        // a non-adjacent pair is separated by an intervening field and would
        // make this test vacuous.
        let mut a = binding();
        a.tool = "a".into();
        a.audience = "b:c".into();
        let mut b = binding();
        b.tool = "a:b".into();
        b.audience = "c".into();
        assert_ne!(
            a.signing_bytes(),
            b.signing_bytes(),
            "length prefixes must stop a character moving between adjacent fields"
        );
    }

    #[test]
    fn the_replay_cache_evicts_entries_older_than_its_window() {
        let mut cache = ReplayCache::new(60);
        assert!(cache.record("n1", 1_000));
        assert_eq!(cache.len(), 1);
        // Far beyond the window: the old nonce is evicted, so the cache cannot
        // grow without bound.
        assert!(cache.record("n2", 5_000));
        assert_eq!(cache.len(), 1);
        // And the evicted nonce is reusable, which is safe: a proof carrying it
        // would fail the age check first.
        assert!(cache.record("n1", 5_000));
    }

    #[test]
    fn thumbprints_are_strictly_parsed() {
        let k = key(1);
        let t = KeyThumbprint::of(&k.verifying_key());
        assert!(KeyThumbprint::parse(t.as_str()).is_ok());
        for bad in [
            "",
            "sha-256:",
            "deadbeef",
            "sha-256:DEADBEEF",
            "sha-1:0000000000000000000000000000000000000000000000000000000000000000",
        ] {
            assert!(
                KeyThumbprint::parse(bad).is_err(),
                "must reject malformed thumbprint {bad:?}"
            );
        }
    }

    #[test]
    fn distinct_keys_have_distinct_thumbprints() {
        assert_ne!(
            KeyThumbprint::of(&key(1).verifying_key()),
            KeyThumbprint::of(&key(2).verifying_key())
        );
    }
}
