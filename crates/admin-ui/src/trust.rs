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

/// One receipt as shown in the Trust Center chain explorer: the decoded body's
/// salient fields plus whether its hash-linkage to the previous receipt holds.
#[derive(Debug, Clone, Serialize)]
pub struct ChainLink {
    /// 0-based position in append order.
    pub index: usize,
    /// Receipt id (UUID).
    pub receipt_id: String,
    /// The receipt verb (`verb.object.state.vN`).
    pub verb: String,
    /// The provider dimension (explicit backend, else verb — see [`receipts`]).
    pub provider: String,
    /// Cost in cents.
    pub cents: u64,
    /// Input tokens billed.
    pub tokens_in: u64,
    /// Output tokens billed.
    pub tokens_out: u64,
    /// Number of tool calls attested.
    pub tool_count: usize,
    /// When the receipted action occurred (ms since epoch).
    pub issued_at_ms: u64,
    /// Whether this receipt's `parent_hash` matches the prior receipt's JWS
    /// digest (genesis: `parent_hash == None`). A `false` here is the exact
    /// index [`ReceiptVerification::error_index`] points at.
    pub link_ok: bool,
}

/// The Trust Center receipt-chain view: the overall verification result plus the
/// most recent links, newest first.
#[derive(Debug, Clone, Serialize)]
pub struct ChainOverview {
    /// Total receipts in the chain.
    pub total: usize,
    /// Whether every link verified.
    pub chain_valid: bool,
    /// First broken index, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_index: Option<usize>,
    /// The most recent links (newest first), capped at the requested limit.
    pub links: Vec<ChainLink>,
}

/// Build the chain explorer view: decode the chain, check each link's parent
/// hash against the prior receipt's JWS digest, and return the newest `limit`
/// links along with the overall verification result. Read-only — no signatures
/// are checked (the admin-ui holds no JWKS; see the module + crate docs).
pub fn chain_overview(receipt_store: &Path, limit: usize) -> anyhow::Result<ChainOverview> {
    let chain = receipts::load_chain(receipt_store)?;
    let mut error_index = None;
    let mut links: Vec<ChainLink> = Vec::with_capacity(chain.len());
    let mut prev: Option<&receipts::LoadedReceipt> = None;
    for (index, receipt) in chain.iter().enumerate() {
        let expected = prev.map(|p| Sha256Digest::of(p.jws_compact.as_bytes()));
        let link_ok = receipt.body.parent_hash == expected;
        if !link_ok && error_index.is_none() {
            error_index = Some(index);
        }
        links.push(ChainLink {
            index,
            receipt_id: receipt.body.receipt_id.to_string(),
            verb: receipt.body.verb.as_str().to_string(),
            provider: receipt.provider().to_string(),
            cents: receipt.body.cost.cents,
            tokens_in: receipt.body.cost.tokens_in,
            tokens_out: receipt.body.cost.tokens_out,
            tool_count: receipt.body.tool_calls.len(),
            issued_at_ms: receipt.body.issued_at.0,
            link_ok,
        });
        prev = Some(receipt);
    }
    let total = links.len();
    links.reverse();
    links.truncate(limit);
    Ok(ChainOverview {
        total,
        chain_valid: error_index.is_none(),
        error_index,
        links,
    })
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
