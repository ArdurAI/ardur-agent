//! Revocation by Biscuit revocation identifier.

use std::collections::HashSet;

use crate::types::CapToken;

/// A revocation oracle consulted by the verifier. A token is rejected if any of
/// its block revocation ids is denied — revoking a parent therefore revokes
/// every token attenuated from it, since the child still carries the parent's
/// block (and thus its revocation id).
pub trait DenyList {
    /// Whether any of `revocation_ids` (a token's per-block ids) is revoked.
    fn is_revoked(&self, revocation_ids: &[Vec<u8>]) -> bool;
}

/// An in-memory [`DenyList`] backed by a hash set of revocation ids.
///
// TODO §11.14 Phase 2: a persisted, shared deny list (the in-memory set does
// not survive a restart and is not visible across verifier instances).
#[derive(Debug, Default, Clone)]
pub struct HashSetDenyList {
    revoked: HashSet<Vec<u8>>,
}

impl HashSetDenyList {
    /// An empty deny list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Revoke a single Biscuit revocation id.
    pub fn revoke(&mut self, revocation_id: Vec<u8>) {
        self.revoked.insert(revocation_id);
    }

    /// Revoke a token by all of its block revocation ids.
    pub fn revoke_token(&mut self, token: &CapToken) {
        self.revoked.extend(token.revocation_ids());
    }
}

impl DenyList for HashSetDenyList {
    fn is_revoked(&self, revocation_ids: &[Vec<u8>]) -> bool {
        revocation_ids.iter().any(|id| self.revoked.contains(id))
    }
}
