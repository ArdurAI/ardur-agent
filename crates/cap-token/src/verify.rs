//! Cap-token verification against a root key, deny list, and concrete request.

use std::time::{Duration, UNIX_EPOCH};

use biscuit_auth::macros::authorizer;
use biscuit_auth::{AuthorizerLimits, Biscuit, PublicKey};

use crate::denylist::DenyList;
use crate::error::{CapTokenError, map_authz_error, map_parse_error};
use crate::types::{CapClaims, CapToken, HolderId, RequiredCaveats, VerifiedClaims};

/// Verify a cap-token (`cap.verify`): check its signature against the issuer
/// root, screen it against a deny list, and authorize it against a concrete
/// request. Returns the issued [`VerifiedClaims`] once every caveat — the
/// authority block's and every attenuation's — is satisfied.
pub trait CapTokenVerifier {
    /// Verify `token` against `root` for the request described by `required`.
    fn verify(
        &self,
        token: &CapToken,
        root: &PublicKey,
        required: &RequiredCaveats,
    ) -> Result<VerifiedClaims, CapTokenError>;
}

/// A [`CapTokenVerifier`] that consults a [`DenyList`] for revocation. The deny
/// list is verifier state (not a per-call argument) so a long-lived verifier
/// can accumulate revocations.
pub struct BiscuitCapTokenVerifier<D: DenyList> {
    deny: D,
}

impl<D: DenyList> BiscuitCapTokenVerifier<D> {
    /// Build a verifier over a deny list.
    pub fn new(deny: D) -> Self {
        Self { deny }
    }

    /// Mutable access to the deny list, for revoking tokens at runtime.
    pub fn deny_list_mut(&mut self) -> &mut D {
        &mut self.deny
    }
}

impl<D: DenyList> CapTokenVerifier for BiscuitCapTokenVerifier<D> {
    fn verify(
        &self,
        token: &CapToken,
        root: &PublicKey,
        required: &RequiredCaveats,
    ) -> Result<VerifiedClaims, CapTokenError> {
        // 1. Re-bind to the supplied root: re-serialize and re-parse so the
        //    block signatures are checked against *this* root, regardless of
        //    which key the in-hand Biscuit was first parsed against.
        let bytes = token.to_bytes()?;
        let biscuit = Biscuit::from(bytes.as_slice(), *root).map_err(map_parse_error)?;

        // 2. Revocation — any denied block id (parent or attenuation) rejects.
        if self.deny.is_revoked(&biscuit.revocation_identifiers()) {
            return Err(CapTokenError::Revoked);
        }

        // 3. Authorize against the request. The authorizer supplies every
        //    request fact the token's checks reference; `allow if true` defers
        //    the verdict entirely to those checks.
        let now = UNIX_EPOCH + Duration::from_secs(required.now_unix);
        let cost = i64::try_from(required.cost).unwrap_or(i64::MAX);
        let authorizer_builder = authorizer!(
            r#"
            time({now});
            audience({audience});
            cost({cost});
            tool({tool});
            allow if true;
            "#,
            now = now,
            audience = required.audience.clone(),
            cost = cost,
            tool = required.tool.clone(),
        );
        // biscuit-auth 6: authorization moved off `Biscuit`. Bind the builder to
        // the token (`build`) then run it (`authorize`); 5.x's
        // `biscuit.authorize(&authorizer)` no longer exists.
        let mut authorizer = authorizer_builder
            .build(&biscuit)
            .map_err(map_authz_error)?;
        // The default Datalog `max_time` is 1ms, which the "cheap" CI workers
        // (macOS especially) routinely blow past under load — a timeout surfaces
        // as `RunLimit` and is mis-mapped to `Malformed` instead of the real
        // caveat verdict. A wider ceiling only buys wall-clock; the logical
        // verdict (which checks pass/fail) is unchanged. biscuit's own test
        // suite raises this to 10s for the same reason.
        let limits = AuthorizerLimits {
            max_time: Duration::from_secs(5),
            ..AuthorizerLimits::default()
        };
        authorizer
            .authorize_with_limits(limits)
            .map_err(map_authz_error)?;

        // 4. Read back the issued claims from the signed authority context.
        let json = biscuit
            .context()
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| CapTokenError::Malformed("missing claims context".to_string()))?;
        let claims: CapClaims = serde_json::from_str(&json)
            .map_err(|e| CapTokenError::Malformed(format!("claims json: {e}")))?;

        Ok(VerifiedClaims {
            token_id: claims.token_id,
            audience: claims.audience,
            subject: HolderId(claims.subject),
            expires_unix: claims.expires_unix,
            budget_remaining: claims.budget_remaining,
            tool_allowlist: claims.tool_allowlist,
            capabilities: claims.capabilities,
        })
    }
}
