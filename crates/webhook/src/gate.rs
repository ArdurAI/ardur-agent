//! Cap-token gating + receipt emission for the operator webhook surface (§9.7).
//!
//! Every endpoint-registry and emit action admits through a cap-token scope
//! check and emits a signed, hash-chained receipt. Endpoint CRUD needs
//! [`SCOPE_ENDPOINT_REGISTER`]; listing needs [`SCOPE_ENDPOINT_READ`]; emits
//! need [`SCOPE_OUTBOUND_EMIT`]; inbound trigger CRUD needs
//! [`SCOPE_INBOUND_REGISTER`]. This closes the "an LLM agent registers a new
//! endpoint pointing at an attacker URL" attack class: endpoint registration
//! is itself policy-gated.

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

use crate::error::WebhookError;

/// Scope required to register / update / revoke outbound endpoints.
pub const SCOPE_ENDPOINT_REGISTER: &str = "webhook.endpoint.register";
/// Scope required to list / read endpoints and triggers.
pub const SCOPE_ENDPOINT_READ: &str = "webhook.endpoint.read";
/// Scope required to emit an outbound webhook.
pub const SCOPE_OUTBOUND_EMIT: &str = "webhook.outbound.emit";
/// Scope required to register / remove inbound triggers.
pub const SCOPE_INBOUND_REGISTER: &str = "webhook.inbound.register";

/// A verified operator identity plus its effective scope set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// The cap-token holder subject.
    pub subject: String,
    /// The verified token id (receipt `cap_token_id`).
    pub token_id: String,
    /// Stable short fingerprint of the subject, for owner-scoped access.
    pub fingerprint: String,
    /// The effective (attenuation-narrowed) scope set.
    pub scopes: Vec<String>,
}

impl Principal {
    /// Whether the principal holds `scope`.
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }

    /// Refuse unless the principal holds `scope`.
    pub fn require(&self, scope: &str) -> Result<(), WebhookError> {
        if self.has(scope) {
            Ok(())
        } else {
            Err(WebhookError::Denied(format!("missing scope `{scope}`")))
        }
    }
}

/// Compute the stable owner fingerprint for a subject.
pub fn fingerprint(subject: &str) -> String {
    let hex = Sha256Digest::of(subject.as_bytes()).to_hex();
    hex[..16].to_string()
}

/// Verifies operator cap-tokens against the issuer root and webhook audience.
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

    /// Verify a base64 cap-token against the baseline [`SCOPE_ENDPOINT_READ`]
    /// and return the verified [`Principal`]. Additional grants (register /
    /// emit / inbound) are checked via [`Principal::has`].
    pub fn authorize(&self, cap_token_b64: &str, now_unix: u64) -> Result<Principal, WebhookError> {
        let token = CapToken::from_base64(cap_token_b64, &self.cap_root)
            .map_err(|e| WebhookError::CapToken(e.to_string()))?;
        let verifier = BiscuitCapTokenVerifier::new(HashSetDenyList::new());
        let claims = verifier
            .verify(
                &token,
                &self.cap_root,
                &RequiredCaveats {
                    now_unix,
                    audience: self.audience.clone(),
                    tool: SCOPE_ENDPOINT_READ.to_string(),
                    cost: 1,
                },
            )
            .map_err(|e| WebhookError::Denied(format!("cap-token: {e}")))?;
        let subject = claims.subject.0.clone();
        Ok(Principal {
            fingerprint: fingerprint(&subject),
            token_id: claims.token_id.to_string(),
            subject,
            scopes: claims.tool_allowlist,
        })
    }
}

/// A webhook operator action to be receipted.
#[derive(Debug, Clone, Copy)]
pub struct ReceiptEvent<'a> {
    /// The 3-segment `verb.object.state.vN` verb.
    pub verb: &'a str,
    /// The cap-token holder subject.
    pub subject: &'a str,
    /// The verified cap-token id.
    pub token_id: &'a str,
    /// Opaque payload (only its digest lands in the receipt body).
    pub payload: &'a [u8],
}

/// Emits signed, hash-chained receipts for webhook operator actions.
pub trait ReceiptSink: Send + Sync {
    /// Emit one receipt; returns the new receipt id.
    fn emit(&self, event: ReceiptEvent<'_>) -> Result<String, WebhookError>;
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

fn last_line_hash(path: &Path) -> Result<Option<Sha256Digest>, WebhookError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .map(|line| Sha256Digest::of(line.as_bytes()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(WebhookError::Io(e.to_string())),
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
    fn emit(&self, event: ReceiptEvent<'_>) -> Result<String, WebhookError> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| WebhookError::Receipt("poisoned lock".to_string()))?;
        let parent_hash = last_line_hash(&self.log_path)?;
        let receipt_id = Uuid::new_v4();
        let body = ReceiptBody {
            receipt_id,
            parent_hash,
            verb: VerbObject::new(event.verb).map_err(|e| WebhookError::Receipt(e.to_string()))?,
            issued_at: UnixTsMillis(now_millis()),
            subject: HolderId(event.subject.to_string()),
            cap_token_id: TokenId(event.token_id.to_string()),
            payload_digest: Sha256Digest::of(event.payload),
            session_id: None,
            cost: CostTuple {
                tokens_in: 0,
                tokens_out: 0,
                cents: 0,
                wall_ms: 0,
                attention_score: 0.0,
            },
            tool_calls: Vec::new(),
            provider: None,
        };
        let signed = ReceiptSigner::sign(body, &self.key)
            .map_err(|e| WebhookError::Receipt(e.to_string()))?;
        if let Some(parent) = self.log_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| WebhookError::Io(e.to_string()))?;
        }
        let mut line = signed.jws_compact().to_string();
        line.push('\n');
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .map_err(|e| WebhookError::Io(e.to_string()))?;
        file.write_all(line.as_bytes())
            .map_err(|e| WebhookError::Io(e.to_string()))?;
        file.sync_all()
            .map_err(|e| WebhookError::Io(e.to_string()))?;
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
    fn emit(&self, event: ReceiptEvent<'_>) -> Result<String, WebhookError> {
        VerbObject::new(event.verb).map_err(|e| WebhookError::Receipt(e.to_string()))?;
        let id = Uuid::new_v4().to_string();
        self.events
            .lock()
            .map_err(|_| WebhookError::Receipt("poisoned lock".to_string()))?
            .push(RecordedReceipt {
                verb: event.verb.to_string(),
                subject: event.subject.to_string(),
                token_id: event.token_id.to_string(),
            });
        Ok(id)
    }
}
