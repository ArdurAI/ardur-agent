//! Trust-center read models: capability wallet, receipt chain verification, and
//! Cedar policy debugger.

use std::path::Path;

use ardur_cap_token::VerifiedClaims;
use ardur_cedar_policy::{
    ActionRef, CedarPolicyBundle, Decision, EvaluationContext, PolicyBundle, PrincipalRef,
    ResourceRef,
};
use ardur_receipt::Sha256Digest;
use serde::{Deserialize, Serialize};

use crate::receipts;

/// One active capability grant shown in the wallet.
#[derive(Debug, Clone, Serialize)]
pub struct WalletGrant {
    /// Stable cap-token id.
    pub token_id: String,
    /// Holder/principal.
    pub subject: String,
    /// Audience this capability is scoped to.
    pub audience: String,
    /// Allowed tools/actions.
    pub tools: Vec<String>,
    /// Expiry as Unix seconds.
    pub expires_unix: u64,
    /// Budget ceiling from the cap claims.
    pub budget_remaining: u64,
    /// UI label for the revoke affordance.
    pub revoke_button_label: &'static str,
    /// The dashboard is read-only; revocation happens through the runtime API.
    pub revoke_supported: bool,
}

impl From<&VerifiedClaims> for WalletGrant {
    fn from(claims: &VerifiedClaims) -> Self {
        Self {
            token_id: claims.token_id.to_string(),
            subject: claims.subject.0.clone(),
            audience: claims.audience.clone(),
            tools: claims.tool_allowlist.clone(),
            expires_unix: claims.expires_unix,
            budget_remaining: claims.budget_remaining,
            revoke_button_label: "Revoke",
            revoke_supported: false,
        }
    }
}

/// Capability-wallet response.
#[derive(Debug, Clone, Serialize)]
pub struct WalletResponse {
    /// Active (non-expired) grants only.
    pub grants: Vec<WalletGrant>,
}

/// Build the active capability wallet from verified claims.
#[must_use]
pub fn wallet(claims: &[VerifiedClaims], now_unix: u64) -> WalletResponse {
    WalletResponse {
        grants: claims
            .iter()
            .filter(|claims| claims.expires_unix >= now_unix)
            .map(WalletGrant::from)
            .collect(),
    }
}

/// Receipt-chain verification response.
#[derive(Debug, Clone, Serialize)]
pub struct ReceiptVerification {
    /// Number of decoded receipts checked.
    pub receipt_count: usize,
    /// Whether the chain is link-valid.
    pub chain_valid: bool,
    /// First bad index, if invalid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_index: Option<usize>,
    /// Human reason, if invalid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Verify admin-ui's decoded receipt chain using the same parent-hash rule as
/// the fused runtime: genesis has no parent; each child points at SHA256(prev
/// compact JWS).
pub fn verify_receipts(receipt_store: &Path) -> anyhow::Result<ReceiptVerification> {
    let chain = receipts::load_chain(receipt_store)?;
    let mut prev: Option<&receipts::LoadedReceipt> = None;
    for (idx, receipt) in chain.iter().enumerate() {
        let expected = prev.map(|p| Sha256Digest::of(p.jws_compact.as_bytes()));
        if receipt.body.parent_hash != expected {
            return Ok(ReceiptVerification {
                receipt_count: chain.len(),
                chain_valid: false,
                error_index: Some(idx),
                reason: Some(format!("broken parent_hash at receipt index {idx}")),
            });
        }
        prev = Some(receipt);
    }
    Ok(ReceiptVerification {
        receipt_count: chain.len(),
        chain_valid: true,
        error_index: None,
        reason: None,
    })
}

/// Query parameters for the policy debugger endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct PolicyDebugQuery {
    /// Cedar principal reference.
    pub principal: String,
    /// Cedar action reference.
    pub action: String,
    /// Cedar resource reference.
    pub resource: String,
    /// Optional resource attributes as a JSON object.
    pub attributes: Option<String>,
}

/// Policy-debugger response.
#[derive(Debug, Clone, Serialize)]
pub struct PolicyDebugResponse {
    /// Allow, Deny, or Indeterminate.
    pub decision: String,
    /// Matched policy ids, when Cedar reported any.
    pub matched_policy_ids: Vec<String>,
    /// Human explanation.
    pub reason: String,
    /// Number of policies loaded in the bundle.
    pub policy_count: usize,
    /// Echo of the evaluated context.
    pub context: EvaluationContext,
}

/// Explain why a policy bundle allowed or denied an action.
pub fn debug_policy(
    policies: &CedarPolicyBundle,
    query: PolicyDebugQuery,
) -> anyhow::Result<PolicyDebugResponse> {
    let attributes = match query.attributes {
        Some(raw) => serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("attributes must be valid JSON: {e}"))?,
        None => serde_json::Value::Null,
    };
    let context = EvaluationContext {
        principal: PrincipalRef(query.principal),
        action: ActionRef(query.action),
        resource: ResourceRef(query.resource),
        attributes,
    };
    let decision = policies.evaluate(&context);
    let policy_count = policies.policy_count();
    Ok(match decision {
        Decision::Allow { matched_policy_ids } => PolicyDebugResponse {
            decision: "Allow".to_string(),
            matched_policy_ids,
            reason: "request allowed by Cedar policy".to_string(),
            policy_count,
            context,
        },
        Decision::Deny {
            matched_policy_ids,
            reason,
        } => PolicyDebugResponse {
            decision: "Deny".to_string(),
            matched_policy_ids,
            reason,
            policy_count,
            context,
        },
        Decision::Indeterminate { reason } => PolicyDebugResponse {
            decision: "Indeterminate".to_string(),
            matched_policy_ids: Vec::new(),
            reason,
            policy_count,
            context,
        },
    })
}
