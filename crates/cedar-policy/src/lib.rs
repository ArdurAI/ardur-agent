//! ardur-cedar-policy — the Cedar policy-bundle substrate (a thin wrapper
//! around the external `cedar-policy` engine).
//!
//! Plan family: §11.0 (`plans/11.0-gateway-policy-foundation-blueprint.md`).
//!
//! # Phase 1 (this crate)
//!
//! Embedded policy evaluation. A [`PolicyBundle`] is loaded from a
//! [`PolicySource`] (inline Cedar text, a file, or a composite of both),
//! compiled into a Cedar `PolicySet`, and answers [`EvaluationContext`]
//! queries with a [`Decision`]. No remote / dynamic policy fetching yet.
//!
//! - [`PolicyBundle`] — the loadable, queryable bundle contract (the surface
//!   was frozen at §0.0; the bodies land here).
//! - [`CedarPolicyBundle`] — the concrete implementation over the official
//!   `cedar-policy` engine.
//! - [`PolicySource`] / [`EvaluationContext`] / [`Decision`] / [`PolicyError`]
//!   — the load, query, result, and failure surfaces.
//!
//! Phase 2 adds hot-reload, schema validation, and remote policy fetch — see
//! the inline `// TODO §11.0 Phase 2:` markers.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::PathBuf;
use std::str::FromStr;

use cedar_policy::{
    Authorizer, Context, Entities, EntityId, EntityTypeName, EntityUid, PolicySet, Request,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Where a [`PolicyBundle`]'s Cedar policies come from.
///
/// Phase 1 is build-time / local only: inline source, a file on disk, or a
/// composite that merges several sources into one policy set, in order.
//
// TODO §11.0 Phase 2: a `Remote { url, etag }` variant for HTTP policy fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicySource {
    /// Inline Cedar policy source code.
    Embedded(String),
    /// A path to a file containing Cedar policy source code.
    File(PathBuf),
    /// Several sources merged into a single policy set, in order.
    Composite(Vec<PolicySource>),
}

/// A Cedar principal entity reference, e.g. `User::alice` or
/// `AgentSession::sess123` (the canonical quoted form `User::"alice"` is also
/// accepted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipalRef(pub String);

/// A Cedar action entity reference, e.g. `Action::Read`, `Tool::FsRead`, or
/// `Provider::Send`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRef(pub String);

/// A Cedar resource entity reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRef(pub String);

/// A single authorization query against a [`PolicyBundle`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationContext {
    /// The principal making the request.
    pub principal: PrincipalRef,
    /// The action being attempted.
    pub action: ActionRef,
    /// The resource being acted upon.
    pub resource: ResourceRef,
    /// Resource attributes consulted by `when` / `unless` clauses. Must be a
    /// JSON object (`null` is treated as no attributes); it is attached to the
    /// resource entity, so policies reference it as `resource.<key>`.
    pub attributes: Value,
}

/// The outcome of evaluating an [`EvaluationContext`] against a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// The request is permitted.
    Allow {
        /// IDs of the policies that contributed to the allow decision.
        matched_policy_ids: Vec<String>,
    },
    /// The request is denied — either an explicit `forbid` matched or no
    /// `permit` was satisfied (implicit deny).
    Deny {
        /// IDs of the policies that contributed to the deny decision (empty
        /// for an implicit deny).
        matched_policy_ids: Vec<String>,
        /// Human-readable explanation of the denial.
        reason: String,
    },
    /// Evaluation produced errors (e.g. a referenced attribute was absent), so
    /// no safe allow/deny could be reached — Cedar's "errors during
    /// evaluation" case.
    Indeterminate {
        /// Description of the evaluation error(s).
        reason: String,
    },
}

/// Errors raised while loading or evaluating a [`PolicyBundle`].
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// The Cedar policy source failed to parse.
    #[error("policy parse error: {0}")]
    Parse(String),
    /// A policy file could not be read.
    #[error("policy i/o error: {0}")]
    IoError(#[from] std::io::Error),
    /// A principal entity reference was not a valid Cedar UID.
    #[error("invalid principal reference: {0}")]
    InvalidPrincipal(String),
    /// A resource entity reference was not a valid Cedar UID.
    #[error("invalid resource reference: {0}")]
    InvalidResource(String),
    /// A required attribute was absent from the evaluation context.
    #[error("missing attribute: {0}")]
    MissingAttribute(String),
    /// An unexpected internal error from the Cedar engine.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// A loaded set of Cedar policies that can answer authorization queries.
///
/// The contract surface was frozen at §0.0; §11.0 Phase 1 fills in the bodies
/// via [`CedarPolicyBundle`]. The default bodies remain `unimplemented!()` so
/// the trait stays object-safe (`load` carries `where Self: Sized`) and the
/// Phase 0 contract-stability test keeps compiling.
pub trait PolicyBundle {
    /// Load and compile a bundle from `source`.
    fn load(source: PolicySource) -> Result<Self, PolicyError>
    where
        Self: Sized,
    {
        let _ = source;
        unimplemented!("§11.0 Phase 1 provides CedarPolicyBundle::load")
    }

    /// Evaluate `ctx` against the bundle and render a [`Decision`].
    fn evaluate(&self, ctx: &EvaluationContext) -> Decision {
        let _ = ctx;
        unimplemented!("§11.0 Phase 1 provides CedarPolicyBundle::evaluate")
    }

    /// The number of compiled policies in the bundle.
    fn policy_count(&self) -> usize {
        unimplemented!("§11.0 Phase 1 provides CedarPolicyBundle::policy_count")
    }
}

/// A [`PolicyBundle`] backed by the official `cedar-policy` engine.
#[derive(Debug, Clone)]
pub struct CedarPolicyBundle {
    policy_set: PolicySet,
    authorizer: Authorizer,
}

impl PolicyBundle for CedarPolicyBundle {
    fn load(source: PolicySource) -> Result<Self, PolicyError> {
        let text = flatten_source(&source)?;
        let policy_set =
            PolicySet::from_str(&text).map_err(|e| PolicyError::Parse(e.to_string()))?;
        Ok(Self {
            policy_set,
            authorizer: Authorizer::new(),
        })
    }

    fn evaluate(&self, ctx: &EvaluationContext) -> Decision {
        match self.try_evaluate(ctx) {
            Ok(decision) => decision,
            Err(err) => Decision::Indeterminate {
                reason: err.to_string(),
            },
        }
    }

    fn policy_count(&self) -> usize {
        self.policy_set.policies().count()
    }
}

impl CedarPolicyBundle {
    /// The fallible core of [`PolicyBundle::evaluate`]: any error here is
    /// surfaced to the caller as [`Decision::Indeterminate`].
    fn try_evaluate(&self, ctx: &EvaluationContext) -> Result<Decision, PolicyError> {
        let principal =
            parse_entity_ref(&ctx.principal.0).map_err(PolicyError::InvalidPrincipal)?;
        let resource = parse_entity_ref(&ctx.resource.0).map_err(PolicyError::InvalidResource)?;
        let action = parse_entity_ref(&ctx.action.0)
            .map_err(|m| PolicyError::Internal(anyhow::anyhow!("invalid action reference: {m}")))?;

        let entities = build_entities(&principal, &resource, &ctx.attributes)?;
        let request = Request::new(principal, action, resource, Context::empty(), None)
            .map_err(|e| PolicyError::Internal(anyhow::anyhow!(e.to_string())))?;

        let response = self
            .authorizer
            .is_authorized(&request, &self.policy_set, &entities);

        let errors: Vec<String> = response
            .diagnostics()
            .errors()
            .map(|e| e.to_string())
            .collect();
        if !errors.is_empty() {
            return Ok(Decision::Indeterminate {
                reason: errors.join("; "),
            });
        }

        let matched_policy_ids: Vec<String> = response
            .diagnostics()
            .reason()
            .map(|id| id.to_string())
            .collect();

        Ok(match response.decision() {
            cedar_policy::Decision::Allow => Decision::Allow { matched_policy_ids },
            cedar_policy::Decision::Deny => Decision::Deny {
                matched_policy_ids,
                reason: "request denied: a forbid policy matched or no permit was satisfied"
                    .to_string(),
            },
        })
    }
}

/// Flatten a (possibly composite) [`PolicySource`] into one Cedar source
/// string. Composite sources are concatenated in order; each policy already
/// terminates with `;`, so newline-joining keeps them syntactically distinct.
fn flatten_source(source: &PolicySource) -> Result<String, PolicyError> {
    match source {
        PolicySource::Embedded(text) => Ok(text.clone()),
        PolicySource::File(path) => std::fs::read_to_string(path).map_err(PolicyError::from),
        PolicySource::Composite(sources) => {
            let mut combined = String::new();
            for source in sources {
                combined.push_str(&flatten_source(source)?);
                combined.push('\n');
            }
            Ok(combined)
        }
    }
}

/// Parse a `Type::id` reference into a Cedar [`EntityUid`]. Tolerates both the
/// bare form (`User::alice`) and the canonical quoted form (`User::"alice"`),
/// and namespaced types (`Foo::Bar::baz`). Built via
/// [`EntityUid::from_type_name_and_id`] rather than string concatenation, per
/// the Cedar API guidance.
fn parse_entity_ref(raw: &str) -> Result<EntityUid, String> {
    let (type_name, id) = raw
        .rsplit_once("::")
        .ok_or_else(|| format!("entity reference '{raw}' is not of the form Type::id"))?;
    let id = id.trim().trim_matches('"');
    let type_name = EntityTypeName::from_str(type_name.trim())
        .map_err(|e| format!("invalid entity type in '{raw}': {e}"))?;
    Ok(EntityUid::from_type_name_and_id(
        type_name,
        EntityId::new(id),
    ))
}

/// Build the entity store for one request. The evaluation attributes ride on
/// the resource entity, so `when`/`unless` clauses can read `resource.<key>`.
fn build_entities(
    principal: &EntityUid,
    resource: &EntityUid,
    attributes: &Value,
) -> Result<Entities, PolicyError> {
    let attrs = match attributes {
        Value::Null => json!({}),
        Value::Object(_) => attributes.clone(),
        _ => {
            return Err(PolicyError::InvalidResource(
                "evaluation attributes must be a JSON object".to_string(),
            ));
        }
    };

    let resource_uid = resource
        .to_json_value()
        .map_err(|e| PolicyError::Internal(anyhow::anyhow!(e.to_string())))?;
    let principal_uid = principal
        .to_json_value()
        .map_err(|e| PolicyError::Internal(anyhow::anyhow!(e.to_string())))?;

    let mut records = vec![json!({ "uid": resource_uid, "attrs": attrs, "parents": [] })];
    // Avoid a duplicate-entity error when principal and resource share a UID.
    if principal_uid != records[0]["uid"] {
        records.push(json!({ "uid": principal_uid, "attrs": {}, "parents": [] }));
    }

    Entities::from_json_value(Value::Array(records), None)
        .map_err(|e| PolicyError::Internal(anyhow::anyhow!(e.to_string())))
}
