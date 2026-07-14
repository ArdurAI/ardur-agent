//! Shared cap-token scope gating, cost-gate budget admission, and receipt-chain
//! emission for the operator-facing automation surfaces: `ardur schedule`
//! (§9.4 cron management) and `ardur webhook` (§9.7 inbound trigger surface).
//!
//! Both surfaces route every mutation through the same three checks so an
//! operator-visible action is never silently ungated:
//!
//! 1. [`require_token_scope`] — the caller presents a token minted by
//!    `ardur token create`; its stored scope must meet or exceed the action's
//!    minimum.
//! 2. [`admit_and_charge`] — a per-resource cents ceiling (persisted on the
//!    schedule/endpoint record) is checked and debited through
//!    `ardur-cost-gate`'s reserve/finalize admission pipeline before the
//!    action runs.
//! 3. [`append_receipt`] — a signed receipt is appended to the same
//!    `~/.ardur/receipts/chain.jsonl` hash chain every chat turn receipts onto,
//!    so cron fires and webhook triggers are auditable evidence, not just log
//!    lines.

use std::path::Path;

use ardur_cli::CliError;
use ardur_cost_gate::{
    AdmissionRequest, CostAdmissionGate, CostEnvelope, CostTuple as GateCostTuple,
    HolderId as GateHolderId, InMemoryBudgetStore, InMemoryCostAdmissionGate, ModelId, ProviderId,
    Sha256Digest as GateDigest, TokenId as GateTokenId,
};
use ardur_receipt::{
    CostTuple as ReceiptCostTuple, HolderId as ReceiptHolderId, ReceiptBody, ReceiptSigner,
    Sha256Digest as ReceiptDigest, TokenId as ReceiptTokenId, UnixTsMillis, VerbObject,
};
use sha2::Digest as _;

/// Scope tiers a stored token can carry, ordered least to most privileged.
/// Mirrors the free-text `scope` field `ardur token create` writes.
fn scope_rank(scope: &str) -> u8 {
    match scope {
        "admin" => 2,
        "write" => 1,
        _ => 0, // "read" and anything unrecognized are the floor.
    }
}

/// Resolve `token_value` against the `ardur token create` store under `root`,
/// requiring a non-revoked token whose scope meets or exceeds `min_scope`.
/// Returns the token's id (used as the receipt's `cap_token_id`) on success.
pub fn require_token_scope(
    root: &Path,
    token_value: Option<&str>,
    min_scope: &str,
) -> Result<String, CliError> {
    let token_value = token_value.ok_or_else(|| {
        CliError::State(format!(
            "this action requires --token with `{min_scope}` scope or higher; run `ardur token create <label> --scope {min_scope}`"
        ))
    })?;
    let hash_hex = hex::encode(sha2::Sha256::digest(token_value.as_bytes()));
    let tokens_dir = root.join("tokens");
    let entries = std::fs::read_dir(&tokens_dir).map_err(|_| {
        CliError::State("no tokens stored; run `ardur token create` first".to_string())
    })?;
    for entry in entries.flatten() {
        if entry.path().extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let matches_hash = record.get("hash").and_then(|v| v.as_str()) == Some(hash_hex.as_str());
        if !matches_hash {
            continue;
        }
        if record
            .get("revoked")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Err(CliError::State(
                "the presented token has been revoked".to_string(),
            ));
        }
        let scope = record
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("read");
        if scope_rank(scope) < scope_rank(min_scope) {
            return Err(CliError::State(format!(
                "the presented token has scope `{scope}`, but this action requires `{min_scope}` or higher"
            )));
        }
        let token_id = record
            .get("token_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        return Ok(token_id);
    }
    Err(CliError::State(
        "no stored token matches the presented value".to_string(),
    ))
}

/// Admit a `charge_cents` debit against a `ceiling_cents` budget that has
/// already spent `spent_cents`, using `ardur-cost-gate`'s reserve/finalize
/// pipeline. Returns the new `spent_cents` total on success; the caller is
/// responsible for persisting it back onto the schedule/endpoint record.
///
/// The gate's budget store is constructed fresh per call (it is a Phase-1
/// in-memory store per the crate's own documentation) and seeded with the
/// resource's *remaining* balance, so ceiling enforcement is real across CLI
/// invocations even though the store itself does not persist.
pub fn admit_and_charge(
    holder_id: &str,
    ceiling_cents: u64,
    spent_cents: u64,
    charge_cents: u64,
) -> Result<u64, CliError> {
    let remaining = ceiling_cents.saturating_sub(spent_cents);
    let budget = InMemoryBudgetStore::new();
    let gate = InMemoryCostAdmissionGate::new(budget);
    let holder = GateHolderId(holder_id.to_string());
    let cap_token = GateTokenId(uuid::Uuid::new_v4());
    gate.bind_token(cap_token, holder.clone());

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        gate.provision_for(&holder, GateCostTuple::cents(remaining))
            .await
            .map_err(|e| CliError::State(format!("budget provisioning failed: {e}")))?;

        let envelope = CostEnvelope {
            tokens_in_max: 0,
            tokens_out_max: 0,
            cents_max: u32::try_from(charge_cents).unwrap_or(u32::MAX),
            wall_ms_max: 0,
            attention_score_max: 0,
        };
        let request = AdmissionRequest {
            cap_token_id: cap_token,
            projected_envelope: envelope,
            provider_id: ProviderId("automation".to_string()),
            model_id: ModelId("cron-fire".to_string()),
            request_digest: GateDigest::of(holder_id.as_bytes()),
        };
        let reservation = gate.admit(request).await?;
        let actual = GateCostTuple::cents(charge_cents);
        gate.finalize(reservation, actual).await?;
        Ok::<(), CliError>(())
    })?;

    Ok(spent_cents.saturating_add(charge_cents))
}

/// Load the P-256 receipt signing key at `path`, minting and persisting a
/// fresh one on first use. Mirrors `StateDirs::load_or_create_receipt_key`,
/// parameterized over the key path so callers can share `~/.ardur/keys/` (the
/// same key the chat runtime signs turns with) without depending on the whole
/// `StateDirs` surface.
fn load_or_create_receipt_key(path: &Path) -> Result<ardur_receipt::Es256SigningKey, CliError> {
    match ardur_cli::read_string_no_follow(path) {
        Ok(pem) => ardur_receipt::Es256SigningKey::from_pkcs8_pem(&pem).map_err(|e| {
            CliError::State(format!(
                "receipt key at {} is malformed: {e}",
                path.display()
            ))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = ardur_receipt::Es256SigningKey::generate();
            let pem = key
                .to_pkcs8_pem()
                .map_err(|e| CliError::State(format!("could not serialize receipt key: {e}")))?;
            ardur_cli::create_private_file_no_follow(path, pem.as_bytes())?;
            Ok(key)
        }
        Err(e) => Err(CliError::Io(e)),
    }
}

/// Append a signed receipt to the shared `<root>/receipts/chain.jsonl` hash
/// chain, generating/loading the P-256 signing key the same way the chat
/// runtime does under `<root>/keys/receipt.pem`. Returns the minted receipt's
/// id. `root` is the resolved `~/.ardur` (or test-temp-dir) state root.
pub fn append_receipt(
    root: &Path,
    verb: &str,
    holder_id: &str,
    cap_token_id: &str,
    cost_cents: u64,
    payload: &serde_json::Value,
) -> Result<uuid::Uuid, CliError> {
    let keys_dir = root.join("keys");
    let receipts_dir = root.join("receipts");
    std::fs::create_dir_all(&keys_dir)?;
    std::fs::create_dir_all(&receipts_dir)?;
    let receipt_key = load_or_create_receipt_key(&keys_dir.join("receipt.pem"))?;

    let log_path = receipts_dir.join("chain.jsonl");
    let existing = ardur_fused_runtime::load_persisted_chain(&log_path)
        .map_err(|e| CliError::State(format!("loading receipt chain: {e}")))?;
    let parent_hash = existing
        .last()
        .map(|r| ReceiptDigest::of(r.jws_compact.as_bytes()));

    let payload_bytes =
        serde_json::to_vec(payload).map_err(|e| CliError::State(format!("payload: {e}")))?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);

    let body = ReceiptBody {
        receipt_id: uuid::Uuid::new_v4(),
        parent_hash,
        verb: VerbObject::new(verb).map_err(|e| CliError::State(format!("invalid verb: {e}")))?,
        issued_at: UnixTsMillis(now_ms),
        subject: ReceiptHolderId(holder_id.to_string()),
        cap_token_id: ReceiptTokenId(cap_token_id.to_string()),
        payload_digest: ReceiptDigest::of(&payload_bytes),
        session_id: None,
        cost: ReceiptCostTuple {
            tokens_in: 0,
            tokens_out: 0,
            cents: cost_cents,
            wall_ms: 0,
            attention_score: 0.0,
        },
        tool_calls: Vec::new(),
        provider: None,
    };
    let receipt_id = body.receipt_id;
    let signed = ReceiptSigner::sign(body, &receipt_key)
        .map_err(|e| CliError::State(format!("signing receipt: {e}")))?;

    let mut out = String::new();
    for entry in &existing {
        out.push_str(&entry.jws_compact);
        out.push('\n');
    }
    out.push_str(signed.jws_compact());
    out.push('\n');
    ardur_cli::write_private_file_atomic_no_follow(&log_path, out.as_bytes())?;

    Ok(receipt_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write_token(root: &Path, token_value: &str, scope: &str, revoked: bool) -> String {
        let tokens_dir = root.join("tokens");
        std::fs::create_dir_all(&tokens_dir).unwrap();
        let token_id = uuid::Uuid::new_v4().to_string();
        let hash_hex = hex::encode(sha2::Sha256::digest(token_value.as_bytes()));
        let record = serde_json::json!({
            "token_id": token_id,
            "label": "test",
            "scope": scope,
            "hash": hash_hex,
            "created_at": 0,
            "revoked": revoked,
        });
        std::fs::write(
            tokens_dir.join(format!("{token_id}.json")),
            serde_json::to_string_pretty(&record).unwrap(),
        )
        .unwrap();
        token_id
    }

    #[test]
    fn require_token_scope_accepts_sufficient_scope() {
        let dir = temp_root();
        let token_id = write_token(dir.path(), "secret-value", "write", false);
        let resolved = require_token_scope(dir.path(), Some("secret-value"), "write").unwrap();
        assert_eq!(resolved, token_id);
    }

    #[test]
    fn require_token_scope_rejects_insufficient_scope() {
        let dir = temp_root();
        write_token(dir.path(), "secret-value", "read", false);
        let err = require_token_scope(dir.path(), Some("secret-value"), "write").unwrap_err();
        assert!(err.to_string().contains("requires `write`"));
    }

    #[test]
    fn require_token_scope_rejects_revoked_token() {
        let dir = temp_root();
        write_token(dir.path(), "secret-value", "admin", true);
        let err = require_token_scope(dir.path(), Some("secret-value"), "read").unwrap_err();
        assert!(err.to_string().contains("revoked"));
    }

    #[test]
    fn require_token_scope_rejects_missing_token() {
        let dir = temp_root();
        let err = require_token_scope(dir.path(), None, "write").unwrap_err();
        assert!(err.to_string().contains("requires --token"));
    }

    #[test]
    fn require_token_scope_rejects_unknown_value() {
        let dir = temp_root();
        write_token(dir.path(), "secret-value", "admin", false);
        let err = require_token_scope(dir.path(), Some("wrong-value"), "read").unwrap_err();
        assert!(err.to_string().contains("no stored token matches"));
    }

    #[test]
    fn admit_and_charge_within_budget_succeeds() {
        let spent = admit_and_charge("holder-a", 1000, 100, 50).unwrap();
        assert_eq!(spent, 150);
    }

    #[test]
    fn admit_and_charge_over_budget_is_denied() {
        let err = admit_and_charge("holder-b", 1000, 980, 50).unwrap_err();
        assert!(matches!(err, CliError::Runtime(_)));
    }

    #[test]
    fn append_receipt_chains_and_verifies() {
        let dir = temp_root();
        // The descriptor-relative no-follow walk in `secure_io` resolves every
        // path component from `/`, so a macOS `$TMPDIR` path through the
        // `/var` -> `/private/var` symlink must be canonicalized first (real
        // `~/.ardur` roots are already canonical; this is a test-only
        // artifact of where the OS places temp directories).
        let root = dir.path().canonicalize().expect("canonicalize tempdir");
        let payload = serde_json::json!({"schedule_id": "abc"});
        let first_id = append_receipt(
            &root,
            "cron.schedule.created.v1",
            "cli://test",
            "tok-1",
            0,
            &payload,
        )
        .unwrap();
        let second_id = append_receipt(
            &root,
            "cron.schedule.fired.v1",
            "cli://test",
            "tok-1",
            5,
            &payload,
        )
        .unwrap();
        assert_ne!(first_id, second_id);

        let log_path = root.join("receipts").join("chain.jsonl");
        let chain = ardur_fused_runtime::load_persisted_chain(&log_path).unwrap();
        assert_eq!(chain.len(), 2);

        let key_pem = std::fs::read_to_string(root.join("keys").join("receipt.pem")).unwrap();
        let key = ardur_receipt::Es256SigningKey::from_pkcs8_pem(&key_pem).unwrap();
        let jwks = ardur_receipt::Jwks::from_public_key(&key.public_key());
        ardur_fused_runtime::verify_persisted_chain_with_jwks(&chain, &jwks)
            .expect("chained receipts verify");
    }
}
