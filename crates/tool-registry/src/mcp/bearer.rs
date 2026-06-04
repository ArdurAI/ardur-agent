//! Bearer-token admission for the MCP HTTP surface.
//!
//! The server gates every MCP request against an operator-configured allowlist
//! (`ARDUR_MCP_BEARER_TOKENS`). Matching is **constant-time** — the presented
//! token is compared against every entry with [`subtle::ConstantTimeEq`] and the
//! results OR-folded, so neither a length mismatch nor an early byte difference
//! shortens the comparison and leaks which prefix was correct.

use subtle::{Choice, ConstantTimeEq};

/// Strip the `Bearer ` scheme prefix from an `Authorization` header value.
///
/// Returns the raw token, or `None` if the header is absent or not a Bearer
/// credential. The scheme name is matched case-sensitively against the exact
/// `"Bearer "` spelling clients send.
#[must_use]
pub fn extract_bearer_token(authorization: Option<&str>) -> Option<&str> {
    authorization?.strip_prefix("Bearer ")
}

/// Whether `presented` matches any token in `allowlist`, compared in constant
/// time.
///
/// An empty allowlist admits nothing. The fold visits every entry regardless of
/// earlier matches so the running time does not depend on which token (if any)
/// matched.
#[must_use]
pub fn bearer_token_allowed(presented: &str, allowlist: &[String]) -> bool {
    let matched = allowlist.iter().fold(Choice::from(0u8), |acc, token| {
        acc | presented.as_bytes().ct_eq(token.as_bytes())
    });
    matched.into()
}
