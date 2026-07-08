//! Cap-token verification against a root key, deny list, and concrete request.

use std::collections::BTreeSet;
use std::time::{Duration, UNIX_EPOCH};

use biscuit_auth::format::schema;
use biscuit_auth::macros::authorizer;
use biscuit_auth::{AuthorizerLimits, Biscuit, PublicKey};
use prost::Message;

use crate::denylist::DenyList;
use crate::error::{CapTokenError, map_authz_error, map_parse_error};
use crate::types::{CapClaims, CapToken, HolderId, RequiredCaveats, VerifiedClaims};

const CUSTOM_SYMBOL_OFFSET: u64 = 1024;

// Keep in sync with biscuit-auth's default symbol table. The public API exposes
// symbol lookup through `Biscuit::print_block_source`, but not the internal
// signed block data; decoding the public protobuf schema is less brittle than
// parsing printed Datalog. Raw string terms below use these indices plus the
// token's custom symbol table.
const DEFAULT_SYMBOLS: &[&str] = &[
    "read",
    "write",
    "resource",
    "operation",
    "right",
    "time",
    "role",
    "owner",
    "tenant",
    "namespace",
    "user",
    "team",
    "service",
    "admin",
    "email",
    "group",
    "member",
    "ip_address",
    "client",
    "client_ip",
    "domain",
    "path",
    "version",
    "cluster",
    "node",
    "hostname",
    "nonce",
    "query",
];

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
        effective_claims_from_blocks(&bytes, claims)
    }
}

fn effective_claims_from_blocks(
    bytes: &[u8],
    claims: CapClaims,
) -> Result<VerifiedClaims, CapTokenError> {
    let token = schema::Biscuit::decode(bytes)
        .map_err(|e| CapTokenError::Malformed(format!("token protobuf: {e}")))?;

    let authority = decode_block(&token.authority.block)?;
    let mut symbols = authority.symbols.clone();
    let mut expires_unix = claims.expires_unix;
    let mut budget_remaining = claims.budget_remaining;
    let mut tool_allowlist = claims.tool_allowlist.clone();

    for signed_block in token.blocks {
        let block = decode_block(&signed_block.block)?;
        symbols.extend(block.symbols.iter().cloned());
        apply_effective_claims_from_block(
            &block,
            &symbols,
            &mut expires_unix,
            &mut budget_remaining,
            &mut tool_allowlist,
        );
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

fn decode_block(bytes: &[u8]) -> Result<schema::Block, CapTokenError> {
    schema::Block::decode(bytes)
        .map_err(|e| CapTokenError::Malformed(format!("block protobuf: {e}")))
}

fn apply_effective_claims_from_block(
    block: &schema::Block,
    symbols: &[String],
    expires_unix: &mut u64,
    budget_remaining: &mut u64,
    tool_allowlist: &mut Vec<String>,
) {
    for check in &block.checks {
        if !is_positive_one_check(check) {
            // The built-in attenuator emits plain `check if ...` constraints.
            // Do not summarize `reject if` or other non-standard shapes into
            // effective allowlists: authorization already enforced them for the
            // concrete request, but their global claim projection is not the
            // same as a positive allowlist/budget intersection.
            continue;
        }
        for query in &check.queries {
            if let Some(expiry) = expiry_limit_from_query(query, symbols) {
                *expires_unix = (*expires_unix).min(expiry);
            }
            if let Some(budget) = budget_limit_from_query(query, symbols) {
                *budget_remaining = (*budget_remaining).min(budget);
            }
            if let Some(allowed_tools) = tool_allowlist_from_query(query, symbols) {
                tool_allowlist.retain(|tool| allowed_tools.contains(tool));
            }
        }
    }
}

fn is_positive_one_check(check: &schema::Check) -> bool {
    check.kind.unwrap_or(schema::check::Kind::One as i32) == schema::check::Kind::One as i32
}

fn expiry_limit_from_query(rule: &schema::Rule, symbols: &[String]) -> Option<u64> {
    let var = single_variable_predicate(rule, symbols, "time")?;
    rule.expressions
        .iter()
        .find_map(|expr| less_or_equal_date_expr(expr, var))
}

fn budget_limit_from_query(rule: &schema::Rule, symbols: &[String]) -> Option<u64> {
    let var = single_variable_predicate(rule, symbols, "cost")?;
    rule.expressions
        .iter()
        .find_map(|expr| less_or_equal_integer_expr(expr, var))
}

fn tool_allowlist_from_query(rule: &schema::Rule, symbols: &[String]) -> Option<BTreeSet<String>> {
    if let Some(tool) = single_string_predicate(rule, symbols, "tool") {
        return Some(BTreeSet::from([tool]));
    }

    let var = single_variable_predicate(rule, symbols, "tool")?;
    rule.expressions
        .iter()
        .find_map(|expr| contains_string_set_expr(expr, var, symbols))
}

fn single_variable_predicate(rule: &schema::Rule, symbols: &[String], name: &str) -> Option<u32> {
    let pred = single_predicate(rule, symbols, name)?;
    if pred.terms.len() != 1 {
        return None;
    }
    term_variable(&pred.terms[0])
}

fn single_string_predicate(rule: &schema::Rule, symbols: &[String], name: &str) -> Option<String> {
    let pred = single_predicate(rule, symbols, name)?;
    if pred.terms.len() != 1 || !rule.expressions.is_empty() {
        return None;
    }
    term_string(&pred.terms[0], symbols)
}

fn single_predicate<'a>(
    rule: &'a schema::Rule,
    symbols: &[String],
    name: &str,
) -> Option<&'a schema::Predicate> {
    if rule.body.len() != 1 {
        return None;
    }
    let pred = &rule.body[0];
    if symbol_name(pred.name, symbols) == Some(name) {
        Some(pred)
    } else {
        None
    }
}

fn less_or_equal_integer_expr(expr: &schema::Expression, var: u32) -> Option<u64> {
    let [left, right, op] = expr.ops.as_slice() else {
        return None;
    };
    if !is_binary(op, schema::op_binary::Kind::LessOrEqual) || term_variable_op(left) != Some(var) {
        return None;
    }
    term_integer_op(right).and_then(|limit| u64::try_from(limit).ok())
}

fn less_or_equal_date_expr(expr: &schema::Expression, var: u32) -> Option<u64> {
    let [left, right, op] = expr.ops.as_slice() else {
        return None;
    };
    if !is_binary(op, schema::op_binary::Kind::LessOrEqual) || term_variable_op(left) != Some(var) {
        return None;
    }
    term_date_op(right)
}

fn contains_string_set_expr(
    expr: &schema::Expression,
    var: u32,
    symbols: &[String],
) -> Option<BTreeSet<String>> {
    let [left, right, op] = expr.ops.as_slice() else {
        return None;
    };
    if !is_binary(op, schema::op_binary::Kind::Contains) || term_variable_op(right) != Some(var) {
        return None;
    }
    term_string_set_op(left, symbols)
}

fn is_binary(op: &schema::Op, kind: schema::op_binary::Kind) -> bool {
    matches!(
        op.content.as_ref(),
        Some(schema::op::Content::Binary(binary)) if binary.kind == kind as i32
    )
}

fn term_variable_op(op: &schema::Op) -> Option<u32> {
    match op.content.as_ref()? {
        schema::op::Content::Value(term) => term_variable(term),
        _ => None,
    }
}

fn term_integer_op(op: &schema::Op) -> Option<i64> {
    match op.content.as_ref()? {
        schema::op::Content::Value(term) => term_integer(term),
        _ => None,
    }
}

fn term_date_op(op: &schema::Op) -> Option<u64> {
    match op.content.as_ref()? {
        schema::op::Content::Value(term) => term_date(term),
        _ => None,
    }
}

fn term_string_set_op(op: &schema::Op, symbols: &[String]) -> Option<BTreeSet<String>> {
    match op.content.as_ref()? {
        schema::op::Content::Value(term) => term_string_set(term, symbols),
        _ => None,
    }
}

fn term_variable(term: &schema::Term) -> Option<u32> {
    match term.content.as_ref()? {
        schema::term::Content::Variable(var) => Some(*var),
        _ => None,
    }
}

fn term_integer(term: &schema::Term) -> Option<i64> {
    match term.content.as_ref()? {
        schema::term::Content::Integer(value) => Some(*value),
        _ => None,
    }
}

fn term_date(term: &schema::Term) -> Option<u64> {
    match term.content.as_ref()? {
        schema::term::Content::Date(value) => Some(*value),
        _ => None,
    }
}

fn term_string(term: &schema::Term, symbols: &[String]) -> Option<String> {
    match term.content.as_ref()? {
        schema::term::Content::String(index) => {
            symbol_name(*index, symbols).map(ToString::to_string)
        }
        _ => None,
    }
}

fn term_string_set(term: &schema::Term, symbols: &[String]) -> Option<BTreeSet<String>> {
    let set = match term.content.as_ref()? {
        schema::term::Content::Set(set) => set,
        _ => return None,
    };

    set.set
        .iter()
        .map(|term| term_string(term, symbols))
        .collect()
}

fn symbol_name(index: u64, symbols: &[String]) -> Option<&str> {
    if let Some(symbol) = DEFAULT_SYMBOLS.get(usize::try_from(index).ok()?) {
        return Some(*symbol);
    }

    if index < CUSTOM_SYMBOL_OFFSET {
        return None;
    }

    let custom_index = usize::try_from(index - CUSTOM_SYMBOL_OFFSET).ok()?;
    symbols.get(custom_index).map(String::as_str)
}
