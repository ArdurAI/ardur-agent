//! Cedar policy gating for `ardur marketplace`'s mutating verbs
//! (install/update/uninstall/publish/audit).
//!
//! The marketplace CLI is a single-operator tool (no multi-tenant identity
//! system), so the Cedar layer here is not authenticating *who* is asking —
//! it is a configurable, operator-editable capability policy: "which
//! declared capabilities, kinds, and claim shapes may this installer ever
//! admit, independent of what the built-in signature/bounds checks already
//! allow." That is a real gap the built-in checks don't cover today: nothing
//! stops an operator from installing a fully-signed, well-formed manifest
//! that declares `shell_exec` — the built-in checks only verify the
//! manifest is authentic and well-formed, not that its *contents* are
//! acceptable. Cedar fills exactly that gap, and does so without a
//! recompile: an operator drops a `.cedar` file with `forbid` rules and the
//! next invocation honors it.
//!
//! The default bundle (no `--policy`/`ARDUR_MARKETPLACE_POLICY` supplied)
//! permits everything — this feature is opt-in hardening, not a new default
//! restriction, so existing single-user workflows are unaffected.

use std::path::Path;

use ardur_cedar_policy::{
    ActionRef, CedarPolicyBundle, Decision, EvaluationContext, PolicyBundle, PolicySource,
    PrincipalRef, ResourceRef,
};
use ardur_cli::CliError;

/// The built-in permissive default: every marketplace action is allowed
/// unless an operator-supplied policy (composed on top) adds a `forbid`.
const DEFAULT_MARKETPLACE_POLICY: &str = r#"
permit(principal, action, resource);
"#;

/// Load the marketplace's Cedar bundle: the built-in permissive default,
/// plus `extra` (an operator-authored `.cedar` file) composed on top when
/// supplied. A `forbid` in `extra` always wins over the default `permit`
/// (Cedar's `forbid` takes precedence over `permit` regardless of order).
pub(crate) fn load_policy(extra: Option<&Path>) -> Result<CedarPolicyBundle, CliError> {
    let source = match extra {
        Some(path) => PolicySource::Composite(vec![
            PolicySource::Embedded(DEFAULT_MARKETPLACE_POLICY.to_string()),
            PolicySource::File(path.to_path_buf()),
        ]),
        None => PolicySource::Embedded(DEFAULT_MARKETPLACE_POLICY.to_string()),
    };
    CedarPolicyBundle::load(source)
        .map_err(|e| CliError::State(format!("loading marketplace Cedar policy: {e}")))
}

/// The local operator identity Cedar policies evaluate against. This CLI has
/// no multi-user auth system; `ARDUR_MARKETPLACE_PRINCIPAL` lets an operator
/// name themselves for a policy that distinguishes principals (e.g. a
/// shared machine with a `forbid ... when principal != User::"admin"` rule).
pub(crate) fn principal() -> PrincipalRef {
    let name =
        std::env::var("ARDUR_MARKETPLACE_PRINCIPAL").unwrap_or_else(|_| "local-user".to_string());
    PrincipalRef(format!("MarketplacePrincipal::\"{name}\""))
}

/// Evaluate `action` against `resource_id`/`attributes`; `Ok(())` on allow,
/// a descriptive [`CliError::State`] on deny or evaluation error (fail
/// closed — an indeterminate policy blocks the action rather than defaulting
/// open).
pub(crate) fn check(
    bundle: &CedarPolicyBundle,
    action: &str,
    resource_id: &str,
    attributes: serde_json::Value,
) -> Result<(), CliError> {
    let ctx = EvaluationContext {
        principal: principal(),
        action: ActionRef(format!("Action::{action}")),
        resource: ResourceRef(format!("Extension::\"{resource_id}\"")),
        attributes,
    };
    match bundle.evaluate(&ctx) {
        Decision::Allow { .. } => Ok(()),
        Decision::Deny { reason, .. } => Err(CliError::State(format!(
            "policy denied `{action}`: {reason}"
        ))),
        Decision::Indeterminate { reason } => Err(CliError::State(format!(
            "policy evaluation error for `{action}`: {reason}"
        ))),
    }
}
