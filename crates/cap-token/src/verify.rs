//! Cap-token verification against a root key, deny list, and concrete request.

use std::collections::BTreeSet;
use std::time::{Duration, UNIX_EPOCH};

use biscuit_auth::macros::authorizer;
use biscuit_auth::{AuthorizerLimits, Biscuit, PublicKey};

use crate::denylist::DenyList;
use crate::error::{CapTokenError, map_authz_error, map_parse_error};
use crate::types::{CapClaims, CapToken, HolderId, RequiredCaveats, VerifiedClaims};

/// Verify a cap-token (`cap.verify`): check its signature against the issuer
/// root, screen it against a deny list, authorize it against a concrete
/// request, and return the effective claims after attenuation blocks have been
/// intersected back into the issued scope.
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

        // 5. The authority context is root/issued scope. Downstream Cedar checks
        //    consume `VerifiedClaims`, so project the signed attenuation checks
        //    back into those claims instead of handing Cedar a widened view.
        effective_claims_from_blocks(&biscuit, claims)
    }
}

/// Intersect every attenuation block's narrowing checks back into the issued
/// claims, **fail-closed**.
///
/// Block 0 is the authority scope, already reflected in `claims`. Blocks `1..`
/// are attenuations. Each block is rendered to Datalog source through biscuit's
/// own symbol table ([`Biscuit::print_block_source`]) — the token is the source
/// of truth for its symbols, so there is no separately-maintained default-symbol
/// table to drift out of sync. Every statement in an attenuation block must be
/// one of the four narrowing checks the attenuator emits (expiry / budget /
/// tool-allowlist / audience). Anything the projector cannot interpret —
/// an unknown predicate, a non-`check if` statement, a fact, a rule, or a
/// check whose shape it does not recognize — rejects the token rather than
/// leaving a claim un-narrowed. Datalog `authorize` (step 3) still enforces the
/// *current* request, but downstream Cedar trusts `VerifiedClaims` for tools and
/// budget *beyond* the current request, so an un-projected narrowing would be a
/// privilege-widening. Fail-closed removes that gap.
fn effective_claims_from_blocks(
    biscuit: &Biscuit,
    claims: CapClaims,
) -> Result<VerifiedClaims, CapTokenError> {
    let mut expires_unix = claims.expires_unix;
    let mut budget_remaining = claims.budget_remaining;
    let mut tool_allowlist = claims.tool_allowlist.clone();

    for index in 1..biscuit.block_count() {
        let source = biscuit
            .print_block_source(index)
            .map_err(|e| CapTokenError::Malformed(format!("block {index} source: {e}")))?;
        for statement in block_statements(&source)? {
            match classify_attenuation(statement)? {
                Narrowing::Expiry(unix) => expires_unix = expires_unix.min(unix),
                Narrowing::Budget(units) => budget_remaining = budget_remaining.min(units),
                Narrowing::Tools(allowed) => tool_allowlist.retain(|tool| allowed.contains(tool)),
                // Audience is a scalar the authorizer already enforced for the
                // request; `VerifiedClaims.audience` reports the issued audience
                // and is not a set to intersect, so there is nothing to project.
                Narrowing::Audience => {}
            }
        }
    }

    Ok(VerifiedClaims {
        token_id: claims.token_id,
        audience: claims.audience,
        subject: HolderId(claims.subject),
        expires_unix,
        budget_remaining,
        tool_allowlist,
    })
}

/// A recognized narrowing check projected out of one attenuation statement.
enum Narrowing {
    /// `check if time($t), $t <= <date>` — bring expiry forward.
    Expiry(u64),
    /// `check if cost($c), $c <= <int>` — lower the spend ceiling.
    Budget(u64),
    /// `check if tool($x), {..}.contains($x)` — shrink the tool allowlist.
    Tools(BTreeSet<String>),
    /// `check if audience("..")` — pin the audience (enforced, not projected).
    Audience,
}

/// Split a block's Datalog source into trimmed, `;`-stripped statements.
///
/// [`Biscuit::print_block_source`] prints each fact/rule/check on its own line,
/// terminated by `;`. A statement that does not end in `;` is a torn or
/// multi-line term — reject it (fail-closed) rather than guess.
fn block_statements(source: &str) -> Result<Vec<&str>, CapTokenError> {
    let mut statements = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let statement = line
            .strip_suffix(';')
            .ok_or_else(|| unprojectable(line))?
            .trim();
        statements.push(statement);
    }
    Ok(statements)
}

/// Classify one attenuation statement, or reject the token if it is not one of
/// the four narrowing checks the attenuator emits.
fn classify_attenuation(statement: &str) -> Result<Narrowing, CapTokenError> {
    // Only positive `check if` narrowing is projectable. Facts, rules,
    // `check all`, and `reject if` are all uninterpretable here → fail-closed.
    let body = statement
        .strip_prefix("check if ")
        .ok_or_else(|| unprojectable(statement))?
        .trim();

    if let Some(unix) = parse_expiry(body) {
        return Ok(Narrowing::Expiry(unix));
    }
    if let Some(units) = parse_budget(body) {
        return Ok(Narrowing::Budget(units));
    }
    if let Some(tools) = parse_tools(body) {
        return Ok(Narrowing::Tools(tools));
    }
    if is_audience(body) {
        return Ok(Narrowing::Audience);
    }
    Err(unprojectable(statement))
}

fn unprojectable(statement: &str) -> CapTokenError {
    CapTokenError::UnprojectableAttenuation(statement.to_string())
}

/// `time($t), $t <= <rfc3339-date>` → the expiry as Unix seconds.
fn parse_expiry(body: &str) -> Option<u64> {
    let (var, rest) = strip_unary_predicate(body, "time")?;
    let date = strip_less_or_equal(rest, var)?;
    parse_rfc3339_secs(date)
}

/// `cost($c), $c <= <int>` → the budget ceiling as `u64`.
fn parse_budget(body: &str) -> Option<u64> {
    let (var, rest) = strip_unary_predicate(body, "cost")?;
    let literal = strip_less_or_equal(rest, var)?;
    u64::try_from(literal.parse::<i64>().ok()?).ok()
}

/// `tool($x), {"a", "b"}.contains($x)` → the narrowed tool set.
fn parse_tools(body: &str) -> Option<BTreeSet<String>> {
    let (var, rest) = strip_unary_predicate(body, "tool")?;
    let set_literal = rest.strip_suffix(&format!(".contains({var})"))?;
    parse_string_set(set_literal)
}

/// `audience("..")` — recognized (enforced by Datalog), nothing to project.
fn is_audience(body: &str) -> bool {
    body.strip_prefix("audience(\"")
        .and_then(|rest| rest.strip_suffix("\")"))
        .is_some_and(|inner| !inner.contains('"'))
}

/// Parse a leading `name($var), ` and return `($var, rest)` with `rest`
/// trimmed. Returns `None` unless `name` is the whole predicate symbol, the
/// single term is a `$variable`, and a comma separates it from the tail.
fn strip_unary_predicate<'a>(body: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
    let (var, tail) = body
        .strip_prefix(name)?
        .strip_prefix('(')?
        .split_once(')')?;
    let var = var.trim();
    if !is_variable(var) {
        return None;
    }
    let rest = tail.trim_start().strip_prefix(',')?.trim_start();
    Some((var, rest))
}

/// Parse `$var <= <operand>` and return the operand text.
fn strip_less_or_equal<'a>(rest: &'a str, var: &str) -> Option<&'a str> {
    let after_var = rest.strip_prefix(var)?.trim_start();
    Some(after_var.strip_prefix("<=")?.trim())
}

/// A biscuit-printed `$name` variable token.
fn is_variable(term: &str) -> bool {
    term.strip_prefix('$').is_some_and(|name| {
        !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
    })
}

/// Parse a biscuit-printed string set literal (`{"a", "b"}`, or `{,}` for the
/// empty set) into its members. Strings are split on their closing quote before
/// the separator, so a member containing a comma is preserved verbatim; any
/// unbalanced quote or stray token yields `None` (fail-closed) rather than a
/// possibly-wider set.
fn parse_string_set(literal: &str) -> Option<BTreeSet<String>> {
    let inner = literal.strip_prefix('{')?.strip_suffix('}')?.trim();
    // biscuit prints the empty set as `{,}`.
    if inner.is_empty() || inner == "," {
        return Some(BTreeSet::new());
    }

    let mut members = BTreeSet::new();
    let mut remaining = inner;
    loop {
        remaining = remaining.trim_start();
        let (value, tail) = remaining.strip_prefix('"')?.split_once('"')?;
        members.insert(value.to_string());
        remaining = tail.trim_start();
        if remaining.is_empty() {
            return Some(members);
        }
        remaining = remaining.strip_prefix(',')?;
    }
}

/// Parse an RFC 3339 timestamp (biscuit's date-term rendering) to Unix seconds.
fn parse_rfc3339_secs(text: &str) -> Option<u64> {
    let datetime = chrono::DateTime::parse_from_rfc3339(text).ok()?;
    u64::try_from(datetime.timestamp()).ok()
}
