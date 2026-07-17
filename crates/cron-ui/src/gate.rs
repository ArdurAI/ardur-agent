//! Cap-token gating + receipt emission for cron UI actions (§9.4, §11.0).
//!
//! Every cron UI action admits through a cap-token scope check and emits a
//! signed, hash-chained receipt. Read-only by default (ADR-Phase3-278):
//! navigation/inspection need [`SCOPE_VIEW`]; mutations need [`SCOPE_MUTATE`];
//! cross-operator (admin) visibility needs [`SCOPE_ADMIN`].
//!
//! The [`ReceiptSink`] trait keeps the controller testable: production wires
//! [`Es256ReceiptSink`] (signs with the operator's ES256 receipt key and
//! appends to a JSONL log); tests wire [`InMemoryReceiptSink`].

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ardur_cap_token::{
    BiscuitCapTokenVerifier, CapToken, CapTokenVerifier, HashSetDenyList, PublicKey,
    RequiredCaveats,
};
use ardur_receipt::{
    CostTuple, Es256SigningKey, HolderId, ReceiptBody, ReceiptSigner, Sha256Digest, TokenId,
    UnixTsMillis, VerbObject,
};
use uuid::Uuid;

use crate::error::{CronUiError, Result};

/// Baseline scope required for any cron UI access (navigation/inspection).
pub const SCOPE_VIEW: &str = "cron.ui.view";
/// Scope required to create/edit/pause/resume/delete crons.
pub const SCOPE_MUTATE: &str = "cron.ui.mutate";
/// Scope required for cross-operator (tenant) visibility.
pub const SCOPE_ADMIN: &str = "cron.ui.admin";
/// Scope required for project-wide visibility.
pub const SCOPE_PROJECT: &str = "cron.ui.project";

/// A verified operator identity plus its effective scope set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// The cap-token holder subject (e.g. `cli://localhost-501`).
    pub subject: String,
    /// The verified token id, used as the receipt `cap_token_id`.
    pub token_id: String,
    /// Stable short fingerprint of the subject, used for owner-scoped
    /// visibility filtering.
    pub fingerprint: String,
    /// The effective (attenuation-narrowed) tool allowlist / scope set.
    pub scopes: Vec<String>,
}

impl Principal {
    /// Whether the principal holds `scope`. Because the verifier returns the
    /// attenuation-narrowed allowlist, membership here is equivalent to the
    /// Biscuit tool-check for that scope.
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// Refuse unless the principal holds `scope`.
    pub fn require(&self, scope: &str) -> Result<()> {
        if self.has(scope) {
            Ok(())
        } else {
            Err(CronUiError::Denied(format!("missing scope `{scope}`")))
        }
    }
}

/// Compute the stable owner fingerprint for a subject.
pub fn fingerprint(subject: &str) -> String {
    let hex = Sha256Digest::of(subject.as_bytes()).to_hex();
    hex[..16].to_string()
}

/// Verifies operator cap-tokens against the issuer root and the cron-UI
/// audience.
#[derive(Debug, Clone)]
pub struct CapGate {
    cap_root: PublicKey,
    audience: String,
}

impl CapGate {
    /// Build a gate over the issuer root public key and the expected audience.
    pub fn new(cap_root: PublicKey, audience: impl Into<String>) -> Self {
        Self {
            cap_root,
            audience: audience.into(),
        }
    }

    /// Verify a base64 cap-token against the baseline [`SCOPE_VIEW`] and return
    /// the verified [`Principal`]. The returned `scopes` carry any additional
    /// grants (mutate/admin) the controller checks via [`Principal::has`].
    pub fn authorize(&self, cap_token_b64: &str, now_unix: u64) -> Result<Principal> {
        let token = CapToken::from_base64(cap_token_b64, &self.cap_root)
            .map_err(|e| CronUiError::CapToken(e.to_string()))?;
        let verifier = BiscuitCapTokenVerifier::new(HashSetDenyList::new());
        let claims = verifier
            .verify(
                &token,
                &self.cap_root,
                &RequiredCaveats {
                    now_unix,
                    audience: self.audience.clone(),
                    tool: SCOPE_VIEW.to_string(),
                    cost: 1,
                },
            )
            .map_err(|e| CronUiError::Denied(format!("cap-token: {e}")))?;
        let subject = claims.subject.0.clone();
        Ok(Principal {
            fingerprint: fingerprint(&subject),
            token_id: claims.token_id.to_string(),
            subject,
            scopes: claims.tool_allowlist,
        })
    }
}

/// A cron-UI action to be receipted.
#[derive(Debug, Clone, Copy)]
pub struct ReceiptEvent<'a> {
    /// The 3-segment `verb.object.state.vN` verb.
    pub verb: &'a str,
    /// The cap-token holder subject.
    pub subject: &'a str,
    /// The verified cap-token id.
    pub token_id: &'a str,
    /// Opaque event payload (only its digest lands in the receipt body).
    pub payload: &'a [u8],
}

/// Emits signed, hash-chained receipts for cron UI actions.
pub trait ReceiptSink: Send + Sync {
    /// Emit one receipt; returns the new receipt id.
    fn emit(&self, event: ReceiptEvent<'_>) -> Result<String>;
}

/// Production receipt sink: signs with an ES256 key and appends the compact JWS
/// to a JSONL log, chaining `parent_hash` to the SHA-256 of the prior line.
pub struct Es256ReceiptSink {
    key: Es256SigningKey,
    log_path: PathBuf,
    lock: Mutex<()>,
}

impl Es256ReceiptSink {
    /// Build a sink over a signing key and a log path.
    pub fn new(key: Es256SigningKey, log_path: impl Into<PathBuf>) -> Self {
        Self {
            key,
            log_path: log_path.into(),
            lock: Mutex::new(()),
        }
    }
}

fn last_line_hash(path: &Path) -> Result<Option<Sha256Digest>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .map(|line| Sha256Digest::of(line.as_bytes()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CronUiError::Io(e.to_string())),
    }
}

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl ReceiptSink for Es256ReceiptSink {
    fn emit(&self, event: ReceiptEvent<'_>) -> Result<String> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| CronUiError::Receipt("poisoned lock".to_string()))?;
        let parent_hash = last_line_hash(&self.log_path)?;
        let receipt_id = Uuid::new_v4();
        let body = ReceiptBody {
            receipt_id,
            parent_hash,
            verb: VerbObject::new(event.verb).map_err(|e| CronUiError::Receipt(e.to_string()))?,
            issued_at: UnixTsMillis(now_millis()),
            subject: HolderId(event.subject.to_string()),
            cap_token_id: TokenId(Uuid::parse_str(&event.token_id).map_err(|e| CronUiError::Receipt(e.to_string()))?),
            payload_digest: Sha256Digest::of(event.payload),
            session_id: None,
            cost: CostTuple {
                tokens_in: 0,
                tokens_out: 0,
                cents: 0,
                wall_ms: 0,
                attention_score: 0,
            },
            tool_calls: Vec::new(),
            provider: None,
        };
        let signed = ReceiptSigner::sign(body, &self.key)
            .map_err(|e| CronUiError::Receipt(e.to_string()))?;
        if let Some(parent) = self.log_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CronUiError::Io(e.to_string()))?;
        }
        let mut line = signed.jws_compact().to_string();
        line.push('\n');
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .map_err(|e| CronUiError::Io(e.to_string()))?;
        file.write_all(line.as_bytes())
            .map_err(|e| CronUiError::Io(e.to_string()))?;
        file.sync_all()
            .map_err(|e| CronUiError::Io(e.to_string()))?;
        Ok(receipt_id.to_string())
    }
}

/// A recorded receipt event (used by [`InMemoryReceiptSink`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedReceipt {
    /// The verb that was emitted.
    pub verb: String,
    /// The subject the receipt was about.
    pub subject: String,
    /// The cap-token id.
    pub token_id: String,
}

/// A test/embedded receipt sink that records events in memory.
#[derive(Debug, Default)]
pub struct InMemoryReceiptSink {
    events: Mutex<Vec<RecordedReceipt>>,
}

impl InMemoryReceiptSink {
    /// Create an empty in-memory sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the recorded events.
    pub fn events(&self) -> Vec<RecordedReceipt> {
        self.events.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

impl ReceiptSink for InMemoryReceiptSink {
    fn emit(&self, event: ReceiptEvent<'_>) -> Result<String> {
        // Reject malformed verbs here too, so tests catch verb-grammar bugs.
        VerbObject::new(event.verb).map_err(|e| CronUiError::Receipt(e.to_string()))?;
        let id = Uuid::new_v4().to_string();
        self.events
            .lock()
            .map_err(|_| CronUiError::Receipt("poisoned lock".to_string()))?
            .push(RecordedReceipt {
                verb: event.verb.to_string(),
                subject: event.subject.to_string(),
                token_id: event.token_id.to_string(),
            });
        Ok(id)
    }
}
