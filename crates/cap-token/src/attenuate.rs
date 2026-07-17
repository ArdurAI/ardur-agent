//! Append-only cap-token attenuation.

use std::collections::BTreeSet;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use biscuit_auth::builder::{Term, string};
use biscuit_auth::macros::block;

use crate::types::{AttenuationRule, CapToken, Caveat};

/// Narrow an existing cap-token by appending a caveat (`cap.attenuate`),
/// producing a child token with strictly narrower authority. Emits
/// `cap.attenuated.v1` once §11.14 wires the event envelope.
pub trait CapTokenAttenuator {
    /// Produce a strictly-narrower child token from `token` under `caveat`.
    fn attenuate(&self, token: &CapToken, caveat: Caveat) -> Result<CapToken>;
}

/// A [`CapTokenAttenuator`] backed by Biscuit block append. Attenuation needs
/// no key material: Biscuit signs each appended block with an ephemeral key
/// chained to the previous block, so a holder narrows a token offline without
/// the issuer's root secret.
#[derive(Debug, Default, Clone, Copy)]
pub struct BiscuitCapTokenAttenuator;

impl CapTokenAttenuator for BiscuitCapTokenAttenuator {
    fn attenuate(&self, token: &CapToken, caveat: Caveat) -> Result<CapToken> {
        // Each rule appends one check over the same request predicates the
        // authority block constrains. Checks intersect, so the child's
        // authority is min(parent, rule) on every axis — never wider.
        let block = match caveat.rule {
            AttenuationRule::RestrictAudience(aud) => {
                block!(r#"check if audience({aud});"#, aud = aud,)
            }
            AttenuationRule::EarlierExpiry(unix) => {
                let exp = UNIX_EPOCH + Duration::from_secs(unix);
                block!(r#"check if time($t), $t <= {exp};"#, exp = exp,)
            }
            AttenuationRule::ReduceBudget(units) => {
                let budget = i64::try_from(units).unwrap_or(i64::MAX);
                block!(r#"check if cost($c), $c <= {budget};"#, budget = budget,)
            }
            AttenuationRule::RestrictTools(tools) => {
                let set: BTreeSet<Term> = tools.iter().map(|t| string(t)).collect();
                block!(r#"check if tool($x), {tools}.contains($x);"#, tools = set,)
            }
        };

        let appended = token.0.append(block)?;
        Ok(CapToken(appended))
    }
}
