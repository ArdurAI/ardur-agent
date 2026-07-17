//! The crate's single typed-error surface.
//!
//! Every fallible verification in this crate returns [`CapTokenError`]. The
//! variants map one-to-one onto the §11.14 failure modes the verifier must
//! distinguish: an expired token is not an audience mismatch is not a forged
//! signature. The first four come from a failed Biscuit check (the verifier
//! attributes the failure by the predicate of the failing rule); the rest come
//! from revocation, signature, or structural decoding.

/// All ways a cap-token operation can fail.
#[derive(Debug, thiserror::Error)]
pub enum CapTokenError {
    /// The request time is past the token's (or an attenuation's) expiry.
    #[error("cap-token expired")]
    Expired,

    /// The presenting audience did not match the issued (or attenuated)
    /// audience.
    #[error("cap-token audience mismatch")]
    AudienceMismatch,

    /// The request's cost exceeded the remaining budget ceiling (issued or
    /// reduced by attenuation).
    #[error("cap-token budget exhausted")]
    BudgetExhausted,

    /// The requested tool was not in the allowlist (issued or narrowed by
    /// attenuation).
    #[error("tool not in cap-token allowlist")]
    ToolNotAllowed,

    /// One of the token's revocation identifiers was present in the deny list.
    #[error("cap-token revoked")]
    Revoked,

    /// The token's block signatures did not verify against the supplied root
    /// public key.
    #[error("cap-token signature did not verify")]
    SignatureInvalid,

    /// The token was structurally invalid — undecodable bytes/base64, or claims
    /// that would not parse.
    #[error("malformed cap-token: {0}")]
    Malformed(String),

    /// An attenuation block carried a check the verifier cannot project into
    /// effective claims. Rejected fail-closed: the token's Datalog still
    /// enforces the current request, but returning the un-narrowed claims could
    /// hand a downstream policy check wider authority (e.g. a tool allowlist)
    /// than the token actually grants. The payload is the offending statement.
    #[error("cap-token carries an uninterpretable attenuation: {0}")]
    UnprojectableAttenuation(String),
}

/// Map a Biscuit parse/verify failure onto a [`CapTokenError`]. A bad signature
/// (wrong root, tampered block, unrecognized key) is [`CapTokenError::Malformed`]'s
/// sibling [`CapTokenError::SignatureInvalid`]; anything else that fails to
/// decode is structural.
pub(crate) fn map_parse_error(e: biscuit_auth::error::Token) -> CapTokenError {
    use biscuit_auth::error::{Format, Token};
    match e {
        Token::Format(Format::Signature(_))
        | Token::Format(Format::SealedSignature)
        | Token::Format(Format::UnknownPublicKey)
        | Token::Format(Format::InvalidSignatureSize(_))
        | Token::Format(Format::SignatureDeserializationError(_))
        | Token::Format(Format::BlockSignatureDeserializationError(_)) => {
            CapTokenError::SignatureInvalid
        }
        other => CapTokenError::Malformed(other.to_string()),
    }
}

/// Map a Biscuit authorization failure onto the specific caveat that rejected
/// the request. Biscuit reports the failed checks as pretty-printed rules; the
/// verifier authors those rules, so the leading predicate (`time` / `audience`
/// / `cost` / `tool`) unambiguously names the violated caveat regardless of
/// whether it came from the authority block or a later attenuation. A
/// signature/structural error here would be a bug (the token was already
/// re-bound to the root), so it degrades to [`CapTokenError::Malformed`].
pub(crate) fn map_authz_error(e: biscuit_auth::error::Token) -> CapTokenError {
    use biscuit_auth::error::{FailedCheck, Logic, Token};

    let checks = match &e {
        Token::FailedLogic(Logic::Unauthorized { checks, .. })
        | Token::FailedLogic(Logic::NoMatchingPolicy { checks }) => checks.as_slice(),
        _ => return CapTokenError::Malformed(e.to_string()),
    };

    let rules: Vec<&str> = checks
        .iter()
        .map(|c| match c {
            FailedCheck::Block(b) => b.rule.as_str(),
            FailedCheck::Authorizer(a) => a.rule.as_str(),
        })
        .collect();

    // Priority: expiry is the most fundamental rejection, then identity
    // (audience), then capability (tool), then quota (budget).
    if rules.iter().any(|r| r.contains("time(")) {
        CapTokenError::Expired
    } else if rules.iter().any(|r| r.contains("audience(")) {
        CapTokenError::AudienceMismatch
    } else if rules.iter().any(|r| r.contains("tool(")) {
        CapTokenError::ToolNotAllowed
    } else if rules.iter().any(|r| r.contains("cost(")) {
        CapTokenError::BudgetExhausted
    } else {
        CapTokenError::Malformed(e.to_string())
    }
}
