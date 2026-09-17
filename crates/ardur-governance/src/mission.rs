//! Mission Declaration authoring (gap G7, decision D2).
//!
//! # Why an MD is needed
//!
//! Conformance requires declared tools, scopes, telemetry and a manifest digest
//! **before** execution — the verifier evaluates `Evaluate(MD, DG,
//! ObservedEvent, LineageState)`, so with no MD there is nothing to evaluate
//! against. ardur-agent's closest analogues are the session config, the operator
//! grant ledger (`~/.ardur/grants.json`) and the skill manifests; this module
//! projects those into a v0.1 Mission Declaration.
//!
//! # Scope: one MD per workspace (decision D2)
//!
//! The MD is workspace-scoped and stable, so it can be cached and its digest
//! pinned; a session narrows it via delegation-grant (DG) narrowing rather than
//! by reissuing a fresh MD per session. Per-session MDs would be more precise
//! but would churn `tool_manifest_digest` constantly, and a digest that changes
//! every session cannot detect manifest drift — §9.6's whole purpose.
//!
//! # What this module will NOT do
//!
//! It never invents authority. An MD is a *declaration of what is permitted*, so
//! every claim is derived from something the operator actually configured:
//!
//! - `allowed_tool_classes` comes from grants that really registered, never from
//!   the full built-in tool list.
//! - `resource_policies` come from grant scopes. A grant with **no** scope
//!   contributes no policy rather than a wildcard — the CLI already refuses to
//!   register scope-less `file.*`/`shell.run` grants, and widening that here
//!   would declare authority the runtime does not grant.
//! - An empty grant ledger yields [`MissionAuthoringError::NoGrantedTools`]
//!   rather than an MD with an empty or wildcard tool list. The schema itself
//!   demands `minItems: 1`; a "deny nothing" MD would be worse than none.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::hash::sha256_hex;
use crate::jcs;

/// The five effect classes the schema requires, in canonical order.
///
/// `effect_policies` is `minItems: 5, maxItems: 5` with a `contains` assertion
/// per class, so all five MUST be present exactly once. Emitting four, or six,
/// or the right count with a duplicate, is a schema violation.
pub const EFFECT_CLASSES: [&str; 5] = ["read", "write", "network", "exec", "external_send"];

/// Telemetry fields this runtime can actually populate on an `ObservedEvent`.
///
/// Declaring a field the emitter cannot fill would be self-sabotage: §9.2 makes
/// every step `insufficient_evidence` the moment a declared field is missing.
/// So this list is deliberately the *intersection* of the schema's enum and what
/// `ardur-observed-events` really emits.
pub const SUPPORTED_TELEMETRY: [&str; 12] = [
    "event_id",
    "session_id",
    "timestamp",
    "actor",
    "action_class",
    "tool_name",
    "target",
    "resource_family",
    "side_effect_class",
    "visibility",
    "sensitivity",
    "grant_id",
];

/// One operator grant, as recorded in `~/.ardur/grants.json`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GrantRecord {
    /// Built-in tool id (e.g. `file.read`).
    pub tool: String,
    /// Capabilities the grant mints.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Confinement root / allowlist. `None` means the grant is unscoped.
    #[serde(default)]
    pub scope: Option<String>,
    /// Operator subject that recorded the grant.
    #[serde(default)]
    pub subject: String,
}

/// Why an MD could not be authored.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MissionAuthoringError {
    /// No grant registered a tool, so there is nothing to declare.
    #[error(
        "no granted tools: an MD must declare at least one tool class, and declaring none \
         (or a wildcard) would misstate the runtime's authority"
    )]
    NoGrantedTools,
    /// A required identity claim was blank.
    #[error("mission identity field `{0}` must not be empty")]
    EmptyIdentity(&'static str),
    /// The declaration could not be canonicalized for hashing.
    #[error("canonicalizing the mission payload failed: {0}")]
    Canonicalization(String),
}

/// A resource policy: which family, which pattern, what sensitivity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePolicy {
    /// Resource namespace used for policy matching (e.g. `fs`).
    pub family: String,
    /// `exact:` or `glob:` prefixed matcher, per the schema pattern.
    pub pattern: String,
    /// Sensitivity tier of the resource.
    pub sensitivity: String,
}

/// A per-class effect ceiling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectPolicy {
    /// One of [`EFFECT_CLASSES`].
    pub side_effect_class: String,
    /// Maximum number of steps in this class.
    pub limit: u64,
}

/// A budget ceiling and the share reservable by children.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetPair {
    /// Mission-wide ceiling.
    pub ceiling: u64,
    /// Portion delegable to children.
    pub reserved_share: u64,
}

/// Mission-wide escrow ceilings keyed by effect class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageBudgets {
    /// One entry per class in [`EFFECT_CLASSES`]; all five are required.
    pub per_effect_class: BTreeMap<String, BudgetPair>,
}

/// How this mission may delegate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationPolicy {
    /// Maximum delegation depth. `0` forbids delegation entirely.
    pub max_depth: u64,
    /// Subject matchers a child may carry.
    pub allowed_child_subjects: Vec<String>,
    /// Narrowing rules a child grant must satisfy.
    pub attenuation_rules: Vec<String>,
}

/// An information-flow rule between content classes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowPolicy {
    /// Source content class.
    pub from_class: String,
    /// Destination content class.
    pub to_class: String,
    /// `allow` or `deny`.
    pub action: String,
}

/// Receipt assurance level required by the mission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptPolicy {
    /// `minimal`, `counter_signed`, or `transparency_logged`.
    pub level: String,
}

/// A memory store the mission governs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernedMemoryStore {
    /// Stable store identifier.
    pub store_id: String,
    /// Resource namespace for policy matching.
    pub resource_family: String,
    /// Retention in seconds.
    pub ttl_s: u64,
    /// `digest_bound`, `entry_signed`, or `transparency_logged`.
    pub integrity_policy: String,
}

/// A v0.1 Mission Declaration claim set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionDeclaration {
    /// Issuer identity.
    pub iss: String,
    /// Subject the mission governs.
    pub sub: String,
    /// Intended audience (the verifier).
    pub aud: String,
    /// Issued-at, NumericDate seconds.
    pub iat: u64,
    /// Expiry, NumericDate seconds.
    pub exp: u64,
    /// Unique declaration id.
    pub jti: String,
    /// Stable mission identifier.
    pub mission_id: String,
    /// Declared tool classes as URIs.
    pub allowed_tool_classes: Vec<String>,
    /// Declared resource policies.
    pub resource_policies: Vec<ResourcePolicy>,
    /// Exactly five effect policies.
    pub effect_policies: Vec<EffectPolicy>,
    /// Mission-wide budgets.
    pub lineage_budgets: LineageBudgets,
    /// Delegation rules.
    pub delegation_policy: DelegationPolicy,
    /// Information-flow rules.
    pub flow_policies: Vec<FlowPolicy>,
    /// Telemetry the verifier may rely on.
    pub required_telemetry: Vec<String>,
    /// Receipt assurance level.
    pub receipt_policy: ReceiptPolicy,
    /// Claimed conformance profile.
    pub conformance_profile: String,
    /// `sha-256:` + 64 lowercase hex over the pinned tool manifest.
    pub tool_manifest_digest: String,
    /// Where revocation state is published.
    pub revocation_ref: String,
    /// Memory stores under governance.
    pub governed_memory_stores: Vec<GovernedMemoryStore>,
}

/// Identity and lifetime inputs the caller supplies.
#[derive(Clone, Debug)]
pub struct MissionIdentity {
    /// Issuer identity.
    pub iss: String,
    /// Subject the mission governs.
    pub sub: String,
    /// Intended audience.
    pub aud: String,
    /// Stable mission identifier (workspace-scoped under D2).
    pub mission_id: String,
    /// Unique declaration id.
    pub jti: String,
    /// Issued-at, NumericDate seconds.
    pub iat: u64,
    /// Expiry, NumericDate seconds.
    pub exp: u64,
    /// Where revocation state is published.
    pub revocation_ref: String,
}

/// Map a built-in tool id to its declared tool-class URI.
///
/// The schema requires a URI (`scheme://`), so a bare `file.read` is not a legal
/// class. Unknown tools still get a URI under the same scheme rather than being
/// dropped: silently omitting a granted tool would understate the mission's real
/// authority, which is the opposite failure from over-declaring but just as
/// dishonest.
fn tool_class_uri(tool: &str) -> String {
    format!("ardur://tool/{tool}")
}

/// The resource family a built-in tool operates in.
fn resource_family(tool: &str) -> &'static str {
    match tool {
        "file.read" | "file.write" | "file.list" => "fs",
        "shell.run" => "process",
        "http.fetch" => "network",
        _ => "other",
    }
}

/// Derive the `sha-256:`-prefixed manifest digest for a tool set.
///
/// Sorted and de-duplicated before hashing so that registry enumeration order
/// cannot read as drift (§9.6 would otherwise fire spuriously), with a separator
/// so `["ab","c"]` and `["a","bc"]` cannot collide.
#[must_use]
pub fn tool_manifest_digest(tool_ids: &[String]) -> String {
    let unique: BTreeSet<&String> = tool_ids.iter().collect();
    let joined: Vec<&str> = unique.iter().map(|s| s.as_str()).collect();
    let payload = joined.join("\u{0}");
    format!("sha-256:{}", sha256_hex(payload.as_bytes()))
}

/// Author a workspace-scoped Mission Declaration from the operator's real state.
///
/// # Errors
///
/// [`MissionAuthoringError::NoGrantedTools`] when no grant registered a tool,
/// and [`MissionAuthoringError::EmptyIdentity`] when a required identity claim
/// is blank. Both are refusals to emit a misleading declaration.
pub fn author_mission_declaration(
    identity: &MissionIdentity,
    grants: &[GrantRecord],
    budgets: &BTreeMap<String, BudgetPair>,
) -> Result<MissionDeclaration, MissionAuthoringError> {
    for (name, value) in [
        ("iss", &identity.iss),
        ("sub", &identity.sub),
        ("aud", &identity.aud),
        ("mission_id", &identity.mission_id),
        ("jti", &identity.jti),
    ] {
        if value.trim().is_empty() {
            return Err(MissionAuthoringError::EmptyIdentity(match name {
                "iss" => "iss",
                "sub" => "sub",
                "aud" => "aud",
                "mission_id" => "mission_id",
                _ => "jti",
            }));
        }
    }

    // Only grants that actually registered a tool contribute authority.
    let mut tools: BTreeSet<String> = BTreeSet::new();
    let mut resource_policies: Vec<ResourcePolicy> = Vec::new();
    for grant in grants {
        let tool = grant.tool.trim();
        if tool.is_empty() {
            continue;
        }
        tools.insert(tool.to_string());

        // A scope-less grant contributes NO resource policy. The CLI refuses to
        // register scope-less file/shell grants, so emitting a wildcard here
        // would declare authority the runtime does not actually grant.
        if let Some(scope) = grant.scope.as_deref().map(str::trim) {
            if !scope.is_empty() {
                let pattern = if scope.contains('*') {
                    format!("glob:{scope}")
                } else {
                    // A confinement ROOT governs everything beneath it, so the
                    // declared pattern must be the subtree, not the bare path —
                    // an `exact:` root would fail to match the files actually
                    // touched under it.
                    format!("glob:{}/**", scope.trim_end_matches('/'))
                };
                let policy = ResourcePolicy {
                    family: resource_family(tool).to_string(),
                    pattern,
                    sensitivity: "internal".to_string(),
                };
                if !resource_policies.contains(&policy) {
                    resource_policies.push(policy);
                }
            }
        }
    }

    if tools.is_empty() {
        return Err(MissionAuthoringError::NoGrantedTools);
    }

    // The schema requires at least one resource policy. When every grant is
    // scope-less we still must not invent one, so declare a policy that matches
    // NOTHING: honest, and it denies rather than permits.
    if resource_policies.is_empty() {
        resource_policies.push(ResourcePolicy {
            family: "none".to_string(),
            pattern: "exact:/dev/null/declared-no-resources".to_string(),
            sensitivity: "internal".to_string(),
        });
    }

    let effect_policies = EFFECT_CLASSES
        .iter()
        .map(|class| EffectPolicy {
            side_effect_class: (*class).to_string(),
            limit: budgets.get(*class).map_or(0, |pair| pair.ceiling),
        })
        .collect();

    // All five classes are required by the schema; a caller that supplied only
    // some gets zeroed ceilings for the rest — a zero ceiling denies, which is
    // the fail-closed direction.
    let per_effect_class = EFFECT_CLASSES
        .iter()
        .map(|class| {
            let pair = budgets.get(*class).cloned().unwrap_or(BudgetPair {
                ceiling: 0,
                reserved_share: 0,
            });
            ((*class).to_string(), pair)
        })
        .collect();

    let allowed_tool_classes = tools.iter().map(|t| tool_class_uri(t)).collect();
    let tool_list: Vec<String> = tools.iter().cloned().collect();

    Ok(MissionDeclaration {
        iss: identity.iss.clone(),
        sub: identity.sub.clone(),
        aud: identity.aud.clone(),
        iat: identity.iat,
        exp: identity.exp,
        jti: identity.jti.clone(),
        mission_id: identity.mission_id.clone(),
        allowed_tool_classes,
        resource_policies,
        effect_policies,
        lineage_budgets: LineageBudgets { per_effect_class },
        delegation_policy: DelegationPolicy {
            // Delegation is OFF by default. #490 explicitly withholds
            // authorization for real provider-backed children, so declaring a
            // non-zero depth would advertise authority the runtime must not use.
            max_depth: 0,
            allowed_child_subjects: Vec::new(),
            // Even with depth 0 the schema requires at least one rule; these are
            // the narrowing invariants any future child must satisfy.
            attenuation_rules: vec![
                "tool_subset".to_string(),
                "resource_subset".to_string(),
                "effect_subset".to_string(),
                "budget_nonincrease".to_string(),
                "telemetry_nonweakening".to_string(),
            ],
        },
        flow_policies: vec![FlowPolicy {
            from_class: "confidential".to_string(),
            to_class: "public".to_string(),
            action: "deny".to_string(),
        }],
        required_telemetry: SUPPORTED_TELEMETRY
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        receipt_policy: ReceiptPolicy {
            // The native chain is self-signed ES256; claiming counter-signed or
            // transparency-logged would overstate the assurance actually provided.
            level: "minimal".to_string(),
        },
        // Delegation-Core is the profile the current evidence supports. Claiming
        // MIC-State or MIC-Evidence would assert coverage that the P0 gap
        // register (G4, G5, G9, G10) says is not yet delivered.
        conformance_profile: "Delegation-Core".to_string(),
        tool_manifest_digest: tool_manifest_digest(&tool_list),
        revocation_ref: identity.revocation_ref.clone(),
        governed_memory_stores: Vec::new(),
    })
}

/// The RFC 8785 canonical digest of an MD payload, as a DG `mission_digest`.
///
/// # Errors
///
/// [`MissionAuthoringError::Canonicalization`] when the payload cannot be
/// canonicalized.
pub fn mission_digest(md: &MissionDeclaration) -> Result<String, MissionAuthoringError> {
    let value = serde_json::to_value(md)
        .map_err(|e| MissionAuthoringError::Canonicalization(e.to_string()))?;
    let bytes = jcs::to_canonical_bytes(&value);
    Ok(format!("sha-256:{}", sha256_hex(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> MissionIdentity {
        MissionIdentity {
            iss: "ardur-agent/cli".into(),
            sub: "cli://localhost".into(),
            aud: "ardur-governance-plane".into(),
            mission_id: "workspace://ardur-agent".into(),
            jti: "01a0adca-0000-4000-8000-00000000000e".into(),
            iat: 1_789_621_936,
            exp: 1_789_708_336,
            revocation_ref: "https://plane.local/revocations".into(),
        }
    }

    fn budgets() -> BTreeMap<String, BudgetPair> {
        EFFECT_CLASSES
            .iter()
            .map(|c| {
                (
                    (*c).to_string(),
                    BudgetPair {
                        ceiling: 100,
                        reserved_share: 10,
                    },
                )
            })
            .collect()
    }

    fn scoped_grants() -> Vec<GrantRecord> {
        vec![
            GrantRecord {
                tool: "file.read".into(),
                capabilities: vec!["cap.fs_read".into()],
                scope: Some("/private/tmp/ardur-beta".into()),
                subject: "cli://localhost".into(),
            },
            GrantRecord {
                tool: "file.write".into(),
                capabilities: vec!["cap.fs_write".into()],
                scope: Some("/private/tmp/ardur-beta".into()),
                subject: "cli://localhost".into(),
            },
        ]
    }

    #[test]
    fn an_md_declares_only_granted_tools() {
        let md = author_mission_declaration(&identity(), &scoped_grants(), &budgets()).unwrap();
        assert_eq!(
            md.allowed_tool_classes,
            vec![
                "ardur://tool/file.read".to_string(),
                "ardur://tool/file.write".to_string()
            ]
        );
    }

    #[test]
    fn an_empty_grant_ledger_is_refused_rather_than_declared_empty() {
        // A wildcard or empty tool list would misstate authority; the schema's
        // minItems:1 agrees.
        assert_eq!(
            author_mission_declaration(&identity(), &[], &budgets()),
            Err(MissionAuthoringError::NoGrantedTools)
        );
    }

    #[test]
    fn a_scope_less_grant_contributes_no_wildcard_resource_policy() {
        let grants = vec![GrantRecord {
            tool: "file.read".into(),
            capabilities: vec!["cap.fs_read".into()],
            scope: None,
            subject: "cli://localhost".into(),
        }];
        let md = author_mission_declaration(&identity(), &grants, &budgets()).unwrap();
        // Exactly one policy, and it matches nothing — never a `glob:/**`.
        assert_eq!(md.resource_policies.len(), 1);
        assert_eq!(md.resource_policies[0].family, "none");
        assert!(
            !md.resource_policies[0].pattern.contains("/**"),
            "a scope-less grant must not widen into a subtree wildcard"
        );
    }

    #[test]
    fn a_scope_root_declares_its_subtree_not_the_bare_path() {
        let md = author_mission_declaration(&identity(), &scoped_grants(), &budgets()).unwrap();
        assert_eq!(
            md.resource_policies[0].pattern, "glob:/private/tmp/ardur-beta/**",
            "a confinement root governs everything beneath it"
        );
    }

    #[test]
    fn exactly_five_effect_policies_one_per_class() {
        let md = author_mission_declaration(&identity(), &scoped_grants(), &budgets()).unwrap();
        assert_eq!(md.effect_policies.len(), 5);
        let classes: BTreeSet<&str> = md
            .effect_policies
            .iter()
            .map(|p| p.side_effect_class.as_str())
            .collect();
        assert_eq!(classes.len(), 5, "no duplicates");
        for class in EFFECT_CLASSES {
            assert!(classes.contains(class), "missing effect class {class}");
        }
    }

    #[test]
    fn missing_budget_classes_default_to_a_zero_ceiling() {
        // Zero denies; omitting the class would fail the schema and inventing a
        // ceiling would grant unearned budget.
        let partial = BTreeMap::from([(
            "read".to_string(),
            BudgetPair {
                ceiling: 50,
                reserved_share: 5,
            },
        )]);
        let md = author_mission_declaration(&identity(), &scoped_grants(), &partial).unwrap();
        let write = md.lineage_budgets.per_effect_class.get("write").unwrap();
        assert_eq!(write.ceiling, 0);
        assert_eq!(write.reserved_share, 0);
    }

    #[test]
    fn delegation_is_declared_off_by_default() {
        // #490 withholds authorization for real children; declaring depth > 0
        // would advertise authority the runtime must not use.
        let md = author_mission_declaration(&identity(), &scoped_grants(), &budgets()).unwrap();
        assert_eq!(md.delegation_policy.max_depth, 0);
        assert!(md.delegation_policy.allowed_child_subjects.is_empty());
        assert!(!md.delegation_policy.attenuation_rules.is_empty());
    }

    #[test]
    fn only_telemetry_the_runtime_can_emit_is_declared() {
        // Declaring a field the emitter cannot fill makes every step
        // insufficient_evidence under §9.2 — self-sabotage.
        let md = author_mission_declaration(&identity(), &scoped_grants(), &budgets()).unwrap();
        for field in &md.required_telemetry {
            assert!(
                SUPPORTED_TELEMETRY.contains(&field.as_str()),
                "declared un-emittable telemetry field {field}"
            );
        }
        assert!(
            !md.required_telemetry
                .contains(&"content_provenance".to_string())
        );
    }

    #[test]
    fn the_claimed_profile_and_receipt_level_do_not_overstate_coverage() {
        let md = author_mission_declaration(&identity(), &scoped_grants(), &budgets()).unwrap();
        assert_eq!(md.conformance_profile, "Delegation-Core");
        assert_eq!(md.receipt_policy.level, "minimal");
    }

    #[test]
    fn blank_identity_claims_are_refused() {
        let mut id = identity();
        id.mission_id = "   ".into();
        assert_eq!(
            author_mission_declaration(&id, &scoped_grants(), &budgets()),
            Err(MissionAuthoringError::EmptyIdentity("mission_id"))
        );
    }

    #[test]
    fn the_manifest_digest_is_order_independent_and_prefixed() {
        let a = tool_manifest_digest(&["file.read".into(), "shell.run".into()]);
        let b = tool_manifest_digest(&["shell.run".into(), "file.read".into()]);
        assert_eq!(a, b, "enumeration order must not read as drift");
        assert!(a.starts_with("sha-256:"), "schema requires the prefix");
        assert_eq!(a.len(), "sha-256:".len() + 64);
        assert_ne!(a, tool_manifest_digest(&["file.read".into()]));
    }

    #[test]
    fn the_manifest_digest_changes_when_a_tool_joins_the_registry() {
        // This is what makes §9.6 manifest-drift detection work at all.
        let md = author_mission_declaration(&identity(), &scoped_grants(), &budgets()).unwrap();
        let mut more = scoped_grants();
        more.push(GrantRecord {
            tool: "shell.run".into(),
            capabilities: vec!["cap.process_exec".into()],
            scope: Some("git|cargo".into()),
            subject: "cli://localhost".into(),
        });
        let md2 = author_mission_declaration(&identity(), &more, &budgets()).unwrap();
        assert_ne!(md.tool_manifest_digest, md2.tool_manifest_digest);
    }

    #[test]
    fn the_mission_digest_is_stable_across_equal_declarations() {
        let a = author_mission_declaration(&identity(), &scoped_grants(), &budgets()).unwrap();
        let b = author_mission_declaration(&identity(), &scoped_grants(), &budgets()).unwrap();
        assert_eq!(mission_digest(&a).unwrap(), mission_digest(&b).unwrap());
    }

    #[test]
    fn the_mission_digest_changes_with_the_declaration() {
        let a = author_mission_declaration(&identity(), &scoped_grants(), &budgets()).unwrap();
        let mut grants = scoped_grants();
        grants.push(GrantRecord {
            tool: "http.fetch".into(),
            capabilities: vec!["cap.net_fetch".into()],
            scope: Some("example.com".into()),
            subject: "cli://localhost".into(),
        });
        let b = author_mission_declaration(&identity(), &grants, &budgets()).unwrap();
        assert_ne!(mission_digest(&a).unwrap(), mission_digest(&b).unwrap());
    }

    #[test]
    fn duplicate_grants_do_not_duplicate_declarations() {
        let mut grants = scoped_grants();
        grants.extend(scoped_grants());
        let md = author_mission_declaration(&identity(), &grants, &budgets()).unwrap();
        assert_eq!(md.allowed_tool_classes.len(), 2);
        // file.read and file.write share one scope and one family, so they
        // collapse to a single policy — deduplication is by policy, not by
        // grant count.
        assert_eq!(md.resource_policies.len(), 1);
        let single = author_mission_declaration(&identity(), &scoped_grants(), &budgets()).unwrap();
        assert_eq!(
            md.resource_policies, single.resource_policies,
            "repeating a grant must not change the declaration"
        );
        assert_eq!(
            md.tool_manifest_digest, single.tool_manifest_digest,
            "repeating a grant must not read as manifest drift"
        );
    }

    #[test]
    fn real_grant_ledger_records_deserialize() {
        // The shape actually written by `ardur grant allow`.
        let json = r#"[{"tool":"file.read","capabilities":["cap.fs_read"],
            "scope":"/private/tmp/ardur-beta","subject":"cli://localhost-502",
            "granted_at_ms":1789622334046,"receipt_id":"4b4be253-f7d5-4bd8-8eb9-bc3efde0dd8e"}]"#;
        let grants: Vec<GrantRecord> = serde_json::from_str(json).expect("ledger parses");
        assert_eq!(grants[0].tool, "file.read");
        assert_eq!(grants[0].scope.as_deref(), Some("/private/tmp/ardur-beta"));
    }
}
