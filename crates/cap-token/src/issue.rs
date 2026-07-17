//! Root cap-token issuance.

use std::collections::BTreeSet;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use biscuit_auth::builder::{Term, string};
use biscuit_auth::macros::biscuit;
use biscuit_auth::{KeyPair, PublicKey};
use uuid::Uuid;

use crate::types::{CapClaims, CapScope, CapToken, HolderId};

/// Issue a root cap-token for a holder from a capability scope (`cap.issue`).
/// Emits `cap.issued.v1` once §11.14 wires the event envelope.
pub trait CapTokenIssuer {
    /// Issue a fresh root token granting `scope` to `holder`.
    fn issue(&self, holder: HolderId, scope: CapScope) -> Result<CapToken>;
}

/// A [`CapTokenIssuer`] that signs tokens with an Ed25519 Biscuit root key.
pub struct BiscuitCapTokenIssuer {
    keypair: KeyPair,
}

impl BiscuitCapTokenIssuer {
    /// Build an issuer over an existing Ed25519 root key pair.
    pub fn new(keypair: KeyPair) -> Self {
        Self { keypair }
    }

    /// The root public key — hand this to a verifier to check tokens this
    /// issuer signs.
    pub fn public_key(&self) -> PublicKey {
        self.keypair.public()
    }
}

impl CapTokenIssuer for BiscuitCapTokenIssuer {
    fn issue(&self, holder: HolderId, scope: CapScope) -> Result<CapToken> {
        let token_id = Uuid::new_v4();
        let expires_at = UNIX_EPOCH + Duration::from_secs(scope.expires_unix);
        let budget = i64::try_from(scope.budget_remaining).unwrap_or(i64::MAX);
        let tool_set: BTreeSet<Term> = scope.tool_allowlist.iter().map(|t| string(t)).collect();

        let claims = CapClaims {
            token_id,
            audience: scope.audience.clone(),
            subject: holder.0.clone(),
            expires_unix: scope.expires_unix,
            budget_remaining: scope.budget_remaining,
            tool_allowlist: scope.tool_allowlist.clone(),
        };
        let context = serde_json::to_string(&claims)?;

        // The authority block carries no grant *facts* — every claim is a check
        // that binds the verifier-supplied request facts (`time`/`audience`/
        // `cost`/`tool`). The issued claims travel in the (signed) block
        // context for read-back. Attenuation only ever appends more such checks.
        let builder = biscuit!(
            r#"
            check if time($t), $t <= {expires};
            check if audience({audience});
            check if cost($c), $c <= {budget};
            check if tool($x), {tools}.contains($x);
            "#,
            expires = expires_at,
            audience = scope.audience,
            budget = budget,
            tools = tool_set,
        );
        // biscuit-auth 6: the builder is consuming — `context` takes/returns
        // `self` (was `set_context(&mut self)` in 5.x).
        let biscuit = builder.context(context).build(&self.keypair)?;
        Ok(CapToken(biscuit))
    }
}
