//! #544 — typed plane-outage classification and idempotent reconnect replay.
//!
//! The Ardur governance plane (`ArdurAI/ardur`, reference proxy in
//! `python/vibap/proxy.py`) is a decision service the fused runtime consults
//! at its existing tool-admission gates. Before B5 activation (#502) the
//! runtime must treat "the plane said no", "the plane is down", "the plane
//! answered garbage" and "the answer may or may not have arrived" as four
//! DIFFERENT states, because only one of them carries the owner-authorized
//! fallback right (#502 B5: native-governed local fallback on plane
//! unreachability — explicitly NOT on a plane denial).
//!
//! This module provides, in dependency order:
//!
//! - [`PlaneOutcome`] — the closed, exhaustive taxonomy above. There is no
//!   wildcard arm anywhere it is matched; a new class is a compile error
//!   until every consumer decides it explicitly.
//! - [`PlaneClient`] — an async HTTP client for `POST /evaluate` that
//!   classifies every failure into [`PlaneOutcome`] fail-closed: an
//!   authenticated decision, a deny-shaped status, or any corrupt/ambiguous
//!   result never falls back; only the enumerated transport shapes do.
//! - The MAC-chained **plane journal** (a `plane/v1` domain namespace,
//!   distinct from the #543 events journal; records are written only AFTER
//!   a decision was observed, so corrupt evidence fails the boot instead of
//!   becoming fall-back-able) with durable event ids and **reconnect
//!   replay**: a reconnect drains the backlog exactly once per event, never
//!   re-runs a tool (the client has no tool access at all) and never
//!   double-debits (the native cost gate remains the only debit authority;
//!   the plane debits nothing here).
//!
//! # Plane revision pin
//!
//! Wire shapes verified against ArdurAI/ardur `dev` @
//! `9cd2f2a5c3f055db84101a6f54b3089991535ab6` (proxy.py lines 7160-7361):
//! `POST /evaluate` returns 200 `{decision: PERMIT|DENY, session_id,
//! reason?}`; `reason == "passport_revoked"` short-circuits to 403
//! `{"error":"passport_revoked"}`; an active kill switch returns 503
//! `{"error":"kill_switch_active"}` on `/evaluate` (and `/session/start`,
//! `/issue`, `/delegate`, `/sessions`) while `/health`, `/result` and
//! `/attest` stay up; auth failures are 401; rate limiting is 429; malformed
//! bodies are 400. A newer plane revision must re-verify these shapes before
//! the fallback class widens.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::hash::hmac_sha256;
use crate::hash::sha256;

/// The proxy's authenticated-deny short-circuit reason (403). Pinned from the
/// plane revision above; compared by exact match, never substring.
pub const PASSPORT_REVOKED: &str = "passport_revoked";

/// The kill-switch 503 error body. Pinned from the plane revision above.
pub const KILL_SWITCH_ACTIVE: &str = "kill_switch_active";

/// The plane's two decision spellings on `POST /evaluate` (200 path).
const DECISION_PERMIT: &str = "PERMIT";
const DECISION_DENY: &str = "DENY";

/// Journal namespace: every MAC in this module is keyed under a label ending
/// in this suffix, so a plane-journal line can never verify as (or be
/// confused with) a #543 evidence-record envelope even though both are
/// HMAC-chained under keys derived from the same custody. Bumping the suffix
/// invalidates every prior journal instead of misinterpreting it.
const PLANE_JOURNAL_DOMAIN: &[u8] = b"ardur-governance/plane-journal/v1";

/// The plane-journal record format written by this build.
pub const PLANE_RECORD_VERSION: u32 = 1;

/// How long an Unreachable observation stays in the same window (seconds).
/// Five minutes matches the operator-scale "one marker per outage" intent;
/// windows are per (key, started) so a flapping plane still produces
/// discrete, countable markers.
pub const OUTAGE_WINDOW_SECS: u64 = 300;

/// The typed taxonomy of "what the plane told us", gh#544 requirement 1.
///
/// Exhaustive by construction: every match site in this crate is wildcard-
/// free, so adding a variant is a compile error until each consumer decides
/// it — exactly the discipline the review that produced #544 asked for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaneOutcome {
    /// The plane answered, authenticated, and permitted the step. Proceed
    /// (every native gate has already passed — this is not a grant).
    Permitted,
    /// The plane answered with an authenticated `DENY`. A denial. Never
    /// fallback-eligible: overriding it would bypass an intentional safety
    /// stop.
    Denied {
        /// The plane's public `reason`, when it sent one.
        reason: Option<String>,
    },
    /// HTTP 403 `passport_revoked` — the authenticated credential was
    /// revoked. A denial (revocation is monotonically applied; treating it
    /// as unavailability would resurrect revoked authority).
    Revoked,
    /// HTTP 503 `{"error":"kill_switch_active"}` — the operator's global
    /// stop. A denial, distinct from every transport failure: the plane is
    /// UP and answering.
    KillSwitch,
    /// The plane could not be consulted at all: connection refused, DNS,
    /// TLS identity error, 404/405/408/501/502/504, or a non-JSON body on
    /// an enumerated no-decision status. **The only owner-authorized
    /// fallback class** (#502 B5): the step proceeds under native
    /// governance and the runtime mints one `governance.plane.unreachable
    /// .v1` marker receipt for the outage window.
    Unreachable {
        /// The stable outage window this observation belongs to.
        window: OutageWindow,
    },
    /// The answer may or may not have been delivered: request timeout, a
    /// 5xx whose body could not be read, or an unclassifiable 5xx. The
    /// step is denied — auto-falling back here would let a mid-request
    /// outage launder a consult into a fallback — and NO unreachable
    /// marker is minted (the plane is not established-unavailable).
    AmbiguousDelivery,
    /// The plane answered with a body that violates its /evaluate contract:
    /// a missing or misspelled `decision`, a decision outside
    /// `PERMIT`/`DENY`, a non-string `reason`, a `reason` riding a PERMIT,
    /// an unexpected status, or a 503 that is not the kill switch. Corrupt
    /// safety state is never fallback-eligible — it is indistinguishable
    /// from a tampered intermediary.
    CorruptEvidence {
        /// What invariant was violated (audit-only; no raw body bytes —
        /// they are untrusted input and must not flow into logs).
        detail: String,
    },
}

impl PlaneOutcome {
    /// Whether this outcome authorizes the owner-granted (#502 B5)
    /// native-governed fallback. Exactly one variant qualifies.
    #[must_use]
    pub fn fallback_eligible(&self) -> bool {
        matches!(self, PlaneOutcome::Unreachable { .. })
    }

    /// Whether this outcome permits the step (the only non-denial answer).
    #[must_use]
    pub fn permits(&self) -> bool {
        matches!(self, PlaneOutcome::Permitted)
    }
}

/// A bounded outage window: `(key, started_unix_secs)`. The key is derived
/// from the plane client's identity material (never a secret) so two
/// clients over one plane share a window only if they share the root key
/// fingerprint and base URL. The runtime debounces marker minting on this
/// value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutageWindow {
    /// `sha256(<root key fingerprint> || <base_url>)` — hex.
    pub key: String,
    /// Unix seconds when this window opened (aligned down to
    /// [`OUTAGE_WINDOW_SECS`] boundaries).
    pub started_unix_secs: u64,
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Derive the outage-window key from public identity material.
fn window_key(root_fingerprint: &[u8; 32], base_url: &str) -> String {
    let mut h = Sha256::new();
    h.update(root_fingerprint);
    h.update(base_url.as_bytes());
    hex_lower(&h.finalize())
}

/// The lifecycle of a journaled plane-consult event, gh#544 requirement 3.
///
/// `Pending → Deliver → (Terminal | DupExplain)`:
///
/// - [`Pending`](Self::Pending) — the consult's authorization inputs were
///   durably recorded BEFORE the request was sent (stable event id, the
///   plane-arguments digest binding, and the exact MD/DG/manifest snapshots
///   the decision is bound to).
/// - [`Deliver`](Self::Deliver) — a request was dispatched at least once
///   with no decision durably journaled yet. A
///   [`PlaneOutcome::AmbiguousDelivery`] terminal result KEEPS the event in
///   this state's backlog: the answer may exist server-side, so a reconnect
///   must ask the plane to explain rather than assume.
/// - [`Terminal`](Self::Terminal) — a decision was observed and journaled.
///   The event is closed; replay never re-sends it.
/// - [`DupExplain`](Self::DupExplain) — a replayed send came back with the
///   same decision the plane already gave (or explained an ambiguous
///   delivery with a definitive answer). Recorded so replay is observably
///   idempotent rather than silently dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PlaneEventStatus {
    /// Inputs durable, request not yet sent.
    Pending,
    /// Request dispatched at least once; no decision durably journaled.
    Deliver,
    /// A decision was observed and journaled.
    Terminal,
    /// A replayed send returned the plane's prior decision (duplicate
    /// delivery detected and explained).
    DupExplain,
}

/// One plane-journal event record. `deny_unknown_fields` + the version pin
/// make a foreign or future record fail closed at load, never interpret.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaneEventRecord {
    /// Record format version ([`PLANE_RECORD_VERSION`]).
    pub v: u32,
    /// The stable durable event identity — the SAME `ev:` identity the #543
    /// evidence journal and the ER chain use for this tool invocation, so
    /// the plane consult, the ER, and any replay agree on one id per event.
    /// Presented to the plane as `risk_request_id` (its idempotency key).
    pub event_id: String,
    /// The session the event belongs to.
    pub session_id: String,
    /// The tool the consult was for.
    pub tool: String,
    /// Hex SHA-256 of the canonical plane arguments (what /evaluate saw).
    /// The arguments themselves are not journaled here — the #543
    /// pre-effect record owns them; this binds the consult to them.
    pub arguments_digest: String,
    /// The Mission-Declaration tool-manifest digest over the registered
    /// tool ids at consult time (exact snapshot, gh#544 requirement 3).
    pub manifest_digest: String,
    /// The Delegation-Grant facts snapshot the consult ran under.
    pub grant: PlaneGrantSnapshot,
    /// Lifecycle status.
    pub status: PlaneEventStatus,
    /// The observed outcome, once one exists (`None` while pending).
    pub outcome: Option<PlaneOutcome>,
    /// When the record was written (Unix milliseconds).
    pub recorded_at_ms: u64,
}

/// The DG snapshot journaled with each consult. Derived from
/// [`crate::GrantDescriptor`] (itself built from the VERIFIED cap-token
/// claims) — a projection of proven facts, never caller-asserted ones.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaneGrantSnapshot {
    /// The verified grant id (cap-token `token_id`; ER/DG `grant_id`).
    pub grant_id: String,
    /// The verified subject.
    pub subject: String,
    /// Effective tool allowlist after attenuation.
    pub effective_tools: Vec<String>,
    /// Effective spend ceiling remaining.
    pub budget_remaining: u64,
    /// Effective expiry (Unix seconds).
    pub expires_unix: u64,
}

impl From<&crate::GrantDescriptor> for PlaneGrantSnapshot {
    fn from(g: &crate::GrantDescriptor) -> Self {
        Self {
            grant_id: g.grant_id.clone(),
            subject: g.subject.clone(),
            effective_tools: g.effective_tools.clone(),
            budget_remaining: g.budget_remaining,
            expires_unix: g.expires_unix,
        }
    }
}

/// The journal line envelope: `{mac, record, seq}`, MAC-chained to its
/// predecessor — the same envelope shape the #543 events journal uses, under
/// a DIFFERENT domain label so the two cannot verify each other's lines.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaneJournalLine {
    mac: String,
    record: String,
    seq: u64,
}

/// The journal's first line: binds every subsequent record to one plane
/// identity (normalized base URL) and root fingerprint, MAC-chained like
/// any other line so the binding itself is tamper-evident (#544 review:
/// without it, repointing `ARDUR_GOVERNANCE_PLANE_URL` while keeping the
/// data dir would let a different plane's decisions close another
/// plane's backlog).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaneJournalHeader {
    v: u32,
    plane: String,
    root_fp: String,
}

/// Failures of the plane client/journal itself (plane verdicts and outages
/// are VALUES, not errors).
#[derive(Debug, thiserror::Error)]
pub enum PlaneClientError {
    /// The configured base URL is not a valid http(s) URL.
    #[error("plane base URL invalid: {0}")]
    InvalidBaseUrl(String),
    /// The journal could not be read, is tampered (MAC/seq failure), or a
    /// record failed closed at parse. The caller fails the boot rather than
    /// replay — corrupt evidence must not become fall-back-able.
    #[error("plane journal corrupt: {0}")]
    CorruptJournal(String),
    /// An i/o failure appending the journal.
    #[error("plane journal i/o failed: {0}")]
    Io(String),
}

/// The in-memory tail of the journal plus the replay backlog.
#[derive(Default)]
struct JournalState {
    next_seq: u64,
    tail_mac: String,
    /// Events awaiting their first send.
    pending: Vec<PlaneEventRecord>,
    /// Events dispatched with no durable decision (response-unknown).
    deliver: Vec<PlaneEventRecord>,
    /// Set when an append's durable outcome is uncertain (write or fsync
    /// failed): the in-memory view may not match disk, so every later
    /// journal operation fails closed until a fresh open re-verifies the
    /// chain (review round 2).
    poisoned: bool,
}

/// A typed client for the plane's `POST /evaluate`, with the MAC-chained
/// consult journal and reconnect replay.
pub struct PlaneClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    window_key: String,
    journal_path: PathBuf,
    mac_key: [u8; 32],
    plane_identity: String,
    root_fp: String,
    journal: Mutex<JournalState>,
}

impl PlaneClient {
    /// Open (or resume) a plane client over `journal_path`.
    ///
    /// The journal is CHECKED, not trusted: every line's MAC and sequence
    /// linkage is verified, every record must parse under THIS build's
    /// version, and any inconsistency returns
    /// [`PlaneClientError::CorruptJournal`] — the caller fails closed.
    ///
    /// # Errors
    /// [`PlaneClientError::InvalidBaseUrl`] for a non-http(s) URL, and
    /// [`PlaneClientError::CorruptJournal`]/[`PlaneClientError::Io`] for an
    /// unreadable or tampered journal.
    pub fn open(
        base_url: &str,
        api_token: &str,
        root_public_key_pem: &str,
        journal_path: &Path,
    ) -> Result<Self, PlaneClientError> {
        Self::open_with_custody(base_url, api_token, root_public_key_pem, None, journal_path)
    }

    /// [`Self::open`], with the ER custody signing key's private PEM as the
    /// journal MAC key source (#544 review: deriving the MAC from the PUBLIC
    /// root PEM gave anyone the key — tamper detection without
    /// authentication). The root PEM stays the identity/window input.
    pub fn open_with_custody(
        base_url: &str,
        api_token: &str,
        root_public_key_pem: &str,
        custody_private_pem: Option<&str>,
        journal_path: &Path,
    ) -> Result<Self, PlaneClientError> {
        let parsed = url::Url::parse(base_url)
            .map_err(|e| PlaneClientError::InvalidBaseUrl(format!("{e} (url: {base_url})")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(PlaneClientError::InvalidBaseUrl(format!(
                "scheme must be http or https, got {}",
                parsed.scheme()
            )));
        }
        // The consult transmits a bearer token and consumes a
        // security-critical PERMIT: plaintext HTTP is refused for any
        // non-loopback host (an on-path attacker could steal the token or
        // replace a denial with a permit). Loopback is the explicit
        // development exception (the wiremock tests live there).
        let host = parsed.host_str().unwrap_or_default();
        let is_loopback = matches!(host, "127.0.0.1" | "::1" | "localhost");
        if parsed.scheme() == "http" && !is_loopback {
            return Err(PlaneClientError::InvalidBaseUrl(format!(
                "refusing plaintext http for non-loopback plane host {host:?}; \
                 use https or an explicit loopback endpoint"
            )));
        }
        // Credential-bearing URLs are refused outright (review round 2):
        // userinfo or a query string can carry passwords/signed tokens
        // that a plain URL log line would disclose.
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(PlaneClientError::InvalidBaseUrl(
                "plane URL must not carry userinfo, query, or fragment".to_string(),
            ));
        }
        let plane_identity = parsed.to_string();
        let mac_key = plane_mac_key(root_public_key_pem, custody_private_pem);
        let root_fp = crate::hash::sha256_hex(root_public_key_pem.as_bytes());
        let window_key = window_key(&sha256(root_public_key_pem.as_bytes()), base_url);
        let journal_path = journal_path.to_path_buf();
        if let Some(parent) = journal_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| PlaneClientError::Io(format!("mkdir: {e}")))?;
        }
        let state = load_journal(&journal_path, &mac_key, &plane_identity, &root_fp)?;
        Ok(Self {
            // rustls-only workspace client (no native-tls); redirects are
            // refused so a decision can never be sourced from a different
            // origin than the pinned base URL.
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| PlaneClientError::Io(format!("http client: {e}")))?,
            base_url: parsed.to_string(),
            token: api_token.to_string(),
            window_key,
            journal_path,
            mac_key,
            plane_identity,
            root_fp,
            journal: Mutex::new(state),
        })
    }

    /// The current outage-window for `now_unix` (the runtime's clock, so
    /// windowing is deterministic under tests).
    #[must_use]
    pub fn window(&self, now_unix: u64) -> OutageWindow {
        OutageWindow {
            key: self.window_key.clone(),
            started_unix_secs: now_unix.saturating_sub(now_unix % OUTAGE_WINDOW_SECS),
        }
    }

    fn evaluate_url(&self) -> String {
        format!("{}/evaluate", self.base_url.trim_end_matches('/'))
    }

    /// Consult the plane for one tool event, gh#544 requirements 1+3.
    ///
    /// Durably records the consult's authorization inputs BEFORE sending
    /// (Pending), journals the dispatch (Deliver), then journals the
    /// observed outcome (Terminal; an AmbiguousDelivery terminal keeps the
    /// event in the replay backlog). The response is classified into
    /// [`PlaneOutcome`] fail-closed; see the type's docs for which classes
    /// exist and which one may fall back.
    ///
    /// The plane endpoint this client consults (for diagnostics/logging).
    pub fn url(&self) -> &str {
        &self.base_url
    }

    /// `event_id` is the #543 stable event identity (`ev:…`), also sent as
    /// the plane's `risk_request_id` idempotency key.
    ///
    /// # Errors
    /// [`PlaneClientError`] only for journal/local-i/o failures — plane
    /// verdicts and outages are VALUES, not errors.
    #[allow(clippy::too_many_arguments)]
    pub async fn evaluate(
        &self,
        event_id: &str,
        session_id: &str,
        tool: &str,
        canonical_arguments: &[u8],
        manifest_digest: &str,
        grant: &crate::GrantDescriptor,
        now_unix: u64,
    ) -> Result<PlaneOutcome, PlaneClientError> {
        let arguments_digest = &crate::hash::sha256_hex(canonical_arguments);
        {
            let state = self.journal.lock().await;
            if state.poisoned {
                return Err(PlaneClientError::CorruptJournal(
                    "journal view poisoned by an earlier uncertain append; \
                     reopen to re-verify the chain"
                        .to_string(),
                ));
            }
            // A deferred backlog (a boot replay that failed, or events
            // stranded by an earlier outage) is retried BEFORE this
            // consult (review round 2): the missed window is re-attested
            // without waiting for a restart. A retry failure logs and the
            // consult proceeds — the backlog stays for the next one.
            if !state.pending.is_empty() || !state.deliver.is_empty() {
                drop(state);
                match self.replay_backlog().await {
                    Ok(replayed) if replayed > 0 => {
                        tracing::info!(replayed, "governance plane backlog replayed")
                    }
                    Ok(_) => {}
                    Err(err) => tracing::warn!(
                        error = %err,
                        "governance plane backlog replay failed; kept for the next consult"
                    ),
                }
            }
        }
        let record = PlaneEventRecord {
            v: PLANE_RECORD_VERSION,
            event_id: event_id.to_string(),
            session_id: session_id.to_string(),
            tool: tool.to_string(),
            arguments_digest: arguments_digest.clone(),
            manifest_digest: manifest_digest.to_string(),
            grant: PlaneGrantSnapshot::from(grant),
            status: PlaneEventStatus::Pending,
            outcome: None,
            recorded_at_ms: now_unix.saturating_mul(1000),
        };
        // Durable intent BEFORE the request: a crash after send but before
        // the outcome journal leaves a Deliver record the replay path can
        // explain, never a silent gap. The Deliver record lands BEFORE the
        // request leaves (review: awaiting the send first left a crash
        // window where bytes reached the plane with only a Pending record —
        // replay would then misrecord the resend as a first delivery).
        self.append(record.clone()).await?;
        let mut delivered = record.clone();
        delivered.status = PlaneEventStatus::Deliver;
        delivered.outcome = None;
        self.append(delivered).await?;
        let outcome = self
            .send_evaluate(event_id, session_id, tool, canonical_arguments, now_unix)
            .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                // A journal failure between send and the terminal write is a
                // LOCAL corruption: classify as ambiguous (the request may
                // have been delivered; we cannot durably say what the plane
                // answered) and surface the journal failure to the caller.
                tracing::warn!(error = %err, "plane journal failure mid-consult");
                return Err(err);
            }
        };
        let mut terminal = record;
        terminal.status = PlaneEventStatus::Terminal;
        terminal.outcome = Some(outcome.clone());
        terminal.recorded_at_ms = now_unix.saturating_mul(1000) + 1;
        self.append(terminal).await?;
        Ok(outcome)
    }

    /// Send the /evaluate request and classify the response. Every shape is
    /// decided; there is no wildcard arm.
    async fn send_evaluate(
        &self,
        event_id: &str,
        session_id: &str,
        tool: &str,
        canonical_arguments: &[u8],
        now_unix: u64,
    ) -> Result<PlaneOutcome, PlaneClientError> {
        // The REAL canonical arguments travel to the plane (review: a
        // digest-only envelope let argument-dependent policy — shell
        // commands, hosts, paths — evaluate a different payload than the
        // tool will run). The stable `risk_request_id` provides
        // idempotency; the digest stays in the durable journal.
        let arguments_value: Value =
            serde_json::from_slice(canonical_arguments).unwrap_or(Value::Null);
        let body = serde_json::json!({
            "session_id": session_id,
            "tool_name": tool,
            "arguments": arguments_value,
            "risk_request_id": event_id,
        });
        let request = self
            .http
            .post(self.evaluate_url())
            .bearer_auth(&self.token)
            .timeout(std::time::Duration::from_secs(10))
            .json(&body);
        let response = match request.send().await {
            Ok(response) => response,
            Err(err) if err.is_timeout() => return Ok(PlaneOutcome::AmbiguousDelivery),
            // ONLY a confirmed connect-phase failure is Unreachable
            // (review round 2): connection refused, DNS, TLS identity —
            // the request provably never left. Every OTHER send error
            // (e.g. a reset after the body was transmitted) leaves the
            // response unknown: the plane may have recorded a decision, so
            // it is AmbiguousDelivery, never fallback.
            Err(err) if err.is_connect() => return Ok(self.unreachable(now_unix)),
            Err(_) => return Ok(PlaneOutcome::AmbiguousDelivery),
        };
        let status = response.status().as_u16();
        // Read the body ONCE, BOUNDED: a malformed or hostile plane must not
        // be able to exhaust memory with an unbounded body. Over-limit or
        // interrupted bodies are corrupt/ambiguous — never fallback.
        let text = match read_body_bounded(response, PLANE_MAX_BODY_BYTES).await {
            Ok(Some(text)) => text,
            Ok(None) => {
                // Over the byte cap: the plane misbehaved on a body we
                // cannot trust — corrupt evidence.
                return Ok(PlaneOutcome::CorruptEvidence {
                    detail: format!(
                        "/evaluate body exceeded the {} byte cap",
                        PLANE_MAX_BODY_BYTES
                    ),
                });
            }
            Err(_) => {
                // ANY interrupted body stream — the decision is unknown
                // whatever the status said (a 200 interrupted after the
                // plane recorded a DENY is the dangerous case). Ambiguous,
                // never unavailable (review P1).
                return Ok(PlaneOutcome::AmbiguousDelivery);
            }
        };
        match status {
            200 => Ok(classify_evaluate_body(&text, session_id)),
            401 | 403 => {
                // Deny-shaped: the plane is up and refused the exchange.
                // 403 + the pinned revoked body is Revoked; anything else in
                // this class is an authenticated denial (the credential is
                // invalid against this plane — not unavailable).
                let code = error_code(&text);
                if status == 403 && code.as_deref() == Some(PASSPORT_REVOKED) {
                    Ok(PlaneOutcome::Revoked)
                } else {
                    Ok(PlaneOutcome::Denied { reason: code })
                }
            }
            429 => Ok(PlaneOutcome::Denied {
                reason: Some("rate_limited".to_string()),
            }),
            503 => {
                let code = error_code(&text);
                if code.as_deref() == Some(KILL_SWITCH_ACTIVE) {
                    Ok(PlaneOutcome::KillSwitch)
                } else {
                    // 503 with a different body: the plane answered, but not
                    // in a shape the pinned revision produces for /evaluate.
                    // Corrupt evidence — never generic unavailability, which
                    // would launder an unknown 503 into fallback.
                    Ok(PlaneOutcome::CorruptEvidence {
                        detail: "503 with unrecognized body".to_string(),
                    })
                }
            }
            400 => {
                // The plane is UP and rejected this client's request as
                // malformed: request-schema drift or a serialization
                // incompatibility. Fail-closed as corrupt evidence —
                // grouping it with transport failures would silently
                // bypass the plane on every consult (review P1).
                Ok(PlaneOutcome::CorruptEvidence {
                    detail: "400 malformed request (client/plane schema mismatch)".to_string(),
                })
            }
            404 | 405 | 408 | 501 => {
                // 404/405/501: the endpoint is not there — enumerated
                // transport refusal. 408: the server closed the request
                // while the client was still sending — no evaluation
                // occurred.
                Ok(self.unreachable(now_unix))
            }
            502 | 504 => {
                // Gateway failures AFTER forwarding (review round 2): the
                // upstream plane may have received, evaluated and recorded
                // a decision before its response was lost. Response
                // unknown — ambiguous, never fallback-eligible.
                Ok(PlaneOutcome::AmbiguousDelivery)
            }
            _ if (500..600).contains(&status) => {
                // Any other 5xx: unknown server failure. The request WAS
                // delivered, so this is ambiguous, not unavailable.
                Ok(PlaneOutcome::AmbiguousDelivery)
            }
            _ => Ok(PlaneOutcome::CorruptEvidence {
                detail: format!("unexpected status {status} from /evaluate"),
            }),
        }
    }

    fn unreachable(&self, now_unix: u64) -> PlaneOutcome {
        PlaneOutcome::Unreachable {
            window: self.window(now_unix),
        }
    }

    /// Append one record to the journal, MAC-chained to the tail, and
    /// maintain the replay backlog in lockstep:
    ///
    /// - Pending adds the event to the pending backlog.
    /// - Deliver moves it to the deliver backlog (response-unknown).
    /// - Terminal closes the event UNLESS the outcome is
    ///   [`PlaneOutcome::AmbiguousDelivery`], which keeps it in the deliver
    ///   backlog for reconnect explanation.
    /// - DupExplain closes it.
    async fn append(&self, record: PlaneEventRecord) -> Result<(), PlaneClientError> {
        let mut state = self.journal.lock().await;
        // Compute the post-append backlog FIRST, apply it ONLY after the
        // durable write succeeds (review round 2): mutating the in-memory
        // lists before `append_locked` reports success let a transient
        // write failure silently drop the event from the live backlog, and
        // a sync failure could leave `next_seq` stale. On any append
        // error the client is POISONED — the durable/observed states may
        // disagree — and every later operation fails closed.
        let mut next = JournalState {
            next_seq: state.next_seq,
            tail_mac: state.tail_mac.clone(),
            pending: state.pending.clone(),
            deliver: state.deliver.clone(),
            poisoned: false,
        };
        let event_id = record.event_id.clone();
        next.pending.retain(|r| r.event_id != event_id);
        next.deliver.retain(|r| r.event_id != event_id);
        match (&record.status, &record.outcome) {
            (PlaneEventStatus::Pending, _) => next.pending.push(record.clone()),
            (PlaneEventStatus::Deliver, _) => next.deliver.push(record.clone()),
            (PlaneEventStatus::Terminal, Some(PlaneOutcome::AmbiguousDelivery))
            | (PlaneEventStatus::Terminal, Some(PlaneOutcome::Unreachable { .. })) => {
                // Non-definitive outcomes never close the event: the
                // reconnect replay must still be able to attest it once
                // the plane actually returns (#544 review).
                next.deliver.push(record.clone());
            }
            (PlaneEventStatus::Terminal, _) | (PlaneEventStatus::DupExplain, _) => {}
        }
        let result = append_locked(
            &self.journal_path,
            &mut next,
            &self.mac_key,
            &self.plane_identity,
            &self.root_fp,
            &record,
        );
        match result {
            Ok(()) => {
                *state = next;
                Ok(())
            }
            Err(err) => {
                // Poison: keep the pre-append view (it may or may not
                // match disk), and refuse every later journal use.
                state.poisoned = true;
                Err(err)
            }
        }
    }

    /// Reconnect replay, gh#544 requirement 3: drain the backlog exactly
    /// once per event. Never re-runs a tool (there is no tool access here),
    /// never re-debits (no debit exists here — the native cost gate is the
    /// only debit authority).
    ///
    /// Events still Pending are sent; events in the deliver backlog
    /// (response unknown) are re-sent with the SAME `risk_request_id` — the
    /// plane's idempotency key — so a decision the plane already recorded is
    /// returned, not re-evaluated. A definitive answer that matches a
    /// previously-journaled definitive outcome (or explains an ambiguous
    /// one) upgrades the record to [`PlaneEventStatus::DupExplain`];
    /// a DEFINITIVE answer that CONTRADICTS a previously-journaled
    /// definitive outcome is journaled as
    /// [`PlaneOutcome::CorruptEvidence`] — the plane changed its mind
    /// outside the protocol, which is exactly the state that must not
    /// silently pass.
    ///
    /// Returns the number of events replayed.
    ///
    /// # Errors
    /// [`PlaneClientError`] on journal/i-o failures (replay stops —
    /// continuing would mint Terminal records we cannot durably account
    /// for).
    pub async fn replay_backlog(&self) -> Result<usize, PlaneClientError> {
        // Snapshot the backlog under the lock, then send outside it (sends
        // are slow; the journal lock guards appends only).
        let backlog: Vec<PlaneEventRecord> = {
            let state = self.journal.lock().await;
            if state.poisoned {
                return Err(PlaneClientError::CorruptJournal(
                    "journal view poisoned by an earlier uncertain append; \
                     reopen to re-verify the chain"
                        .to_string(),
                ));
            }
            state
                .pending
                .iter()
                .chain(state.deliver.iter())
                .cloned()
                .collect()
        };
        let mut replayed = 0;
        for mut record in backlog {
            replayed += 1;
            let now = now_secs();
            // Replay presents the SAME `risk_request_id`: the plane's
            // idempotency key returns its recorded decision instead of
            // re-evaluating. The arguments envelope carries the journaled
            // digest (the canonical bytes are not retained durably — the
            // plane already holds the original request under this key).
            let replay_arguments = serde_json::to_vec(&serde_json::json!({
                "arguments_digest": record.arguments_digest,
            }))
            .map_err(|e| PlaneClientError::Io(format!("serialize: {e}")))?;
            let outcome = self
                .send_evaluate(
                    &record.event_id,
                    &record.session_id,
                    &record.tool,
                    &replay_arguments,
                    now,
                )
                .await?;
            let previous = record.outcome.clone();
            let was_dispatched =
                matches!(record.status, PlaneEventStatus::Deliver) || previous.is_some();
            let (status, outcome) =
                replay_classification(previous.as_ref(), was_dispatched, &outcome);
            record.status = status;
            record.outcome = Some(outcome);
            record.recorded_at_ms = now.saturating_mul(1000);
            self.append(record).await?;
        }
        Ok(replayed)
    }
}

// ---------------------------------------------------------------------------
// classification helpers
// ---------------------------------------------------------------------------

/// The `error` field of a JSON error body, when present and a string.
fn error_code(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
}

/// The current Unix second from the std clock (replay timestamps only — the
/// live consult path carries the caller's clock).
/// The /evaluate response body cap: a governance plane is trusted for
/// decisions, not for buffer discipline (#544 review). Over-cap bodies are
/// corrupt evidence.
const PLANE_MAX_BODY_BYTES: usize = 1 << 20;

/// Read a response body up to `cap` bytes. `Ok(None)` = over cap;
/// `Err` = the stream failed (interrupted body — decision unknown).
async fn read_body_bounded(
    response: reqwest::Response,
    cap: usize,
) -> Result<Option<String>, reqwest::Error> {
    let mut body: Vec<u8> = Vec::with_capacity(1024);
    use futures_util::StreamExt as _;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > cap {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    match String::from_utf8(body) {
        Ok(text) => Ok(Some(text)),
        // Non-utf-8 body: not a shape the pinned revision produces. The
        // empty string cannot match any pinned shape, so classification
        // lands on corrupt evidence — the fail-closed outcome.
        Err(_) => Ok(Some(String::new())),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Classify a 200 body against the pinned /evaluate contract. Corrupt
/// evidence — never a guess, never fallback.
fn classify_evaluate_body(text: &str, expected_session: &str) -> PlaneOutcome {
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(_) => {
            return PlaneOutcome::CorruptEvidence {
                detail: "200 body is not JSON".to_string(),
            };
        }
    };
    let Some(decision) = value.get("decision").and_then(Value::as_str) else {
        return PlaneOutcome::CorruptEvidence {
            detail: "200 body lacks a string `decision`".to_string(),
        };
    };
    // #544 review: the pinned 200 contract carries `session_id`; a PERMIT
    // for a missing or different session (stale/cached/cross-session
    // response) authorizes nothing here — corrupt evidence.
    let session = value.get("session_id").and_then(Value::as_str);
    if session != Some(expected_session) {
        return PlaneOutcome::CorruptEvidence {
            detail: format!(
                "session_id {:?} does not match the consulted session",
                session
            ),
        };
    }
    let reason = value.get("reason");
    match decision {
        DECISION_PERMIT => {
            if let Some(reason) = reason {
                if !reason.is_string() {
                    return PlaneOutcome::CorruptEvidence {
                        detail: "PERMIT carries a non-string `reason`".to_string(),
                    };
                }
                // The pinned contract emits `reason` only on DENY; a PERMIT
                // carrying one is off-contract — corrupt, not compliant.
                return PlaneOutcome::CorruptEvidence {
                    detail: "PERMIT carries a `reason` (contract emits reason only on DENY)"
                        .to_string(),
                };
            }
            PlaneOutcome::Permitted
        }
        DECISION_DENY => {
            let reason = match reason {
                None => None,
                Some(reason) => match reason.as_str() {
                    Some(reason) => Some(reason.to_string()),
                    None => {
                        return PlaneOutcome::CorruptEvidence {
                            detail: "DENY carries a non-string `reason`".to_string(),
                        };
                    }
                },
            };
            PlaneOutcome::Denied { reason }
        }
        other => PlaneOutcome::CorruptEvidence {
            detail: format!("decision {other:?} is neither PERMIT nor DENY"),
        },
    }
}

// ---------------------------------------------------------------------------
// journal
// ---------------------------------------------------------------------------

/// Derive the plane-journal MAC key from the ER signing custody's PEM under
/// a domain-separated label — the same construction as the #543
/// `evidence_record_mac_key`, under the plane label so the two journals
/// cannot verify each other's lines.
fn plane_mac_key(root_public_pem: &str, custody_private_pem: Option<&str>) -> [u8; 32] {
    // #544 review: the journal MAC must be keyed by material an attacker
    // who can rewrite plane.jsonl does NOT hold. The public root PEM alone
    // fails that (it is configuration); the ER custody PRIVATE key is the
    // secret already guarding the sibling evidence journals. When no
    // custody key is available (embedded/test contexts), the derivation is
    // domain-separated over the public root AND flagged by absence — but
    // production wiring (server/CLI) always passes the custody PEM.
    let mut h = Sha256::new();
    h.update(PLANE_JOURNAL_DOMAIN);
    match custody_private_pem {
        Some(secret) => {
            h.update(b"custody\0");
            h.update(secret.as_bytes());
        }
        None => {
            h.update(b"public-root-only\0");
            h.update(root_public_pem.as_bytes());
        }
    }
    h.finalize().into()
}

/// The plane-journal line MAC: covers the sequence number, the previous
/// line's tail MAC (deletion detection), and the record line.
fn hmac_plane_line(key: &[u8; 32], seq: u64, prev_tail_mac: &str, line: &str) -> String {
    hmac_sha256(
        key,
        &[
            b"ardur-governance/plane-line/v1\0",
            &seq.to_be_bytes(),
            prev_tail_mac.as_bytes(),
            line.as_bytes(),
        ],
    )
}

/// Load (and check) the journal. Missing file → empty state; anything else
/// is verified line by line and fails closed on the first inconsistency.
fn load_journal(
    path: &Path,
    key: &[u8; 32],
    plane_identity: &str,
    root_fp: &str,
) -> Result<JournalState, PlaneClientError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Establish the journal durably (file + parent dir fsync) so a
            // crash right after open cannot lose the empty-file fact. The
            // identity header lands with the first appended record.
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| PlaneClientError::Io(format!("create: {e}")))?;
            file.sync_all()
                .map_err(|e| PlaneClientError::Io(format!("fsync: {e}")))?;
            if let Some(parent) = path.parent() {
                if let Ok(dir) = std::fs::File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
            return Ok(JournalState::default());
        }
        Err(err) => return Err(PlaneClientError::Io(format!("read: {err}"))),
    };
    if bytes.is_empty() {
        return Ok(JournalState::default());
    }
    if bytes.last() != Some(&b'\n') {
        return Err(PlaneClientError::CorruptJournal(
            "torn tail (last line lacks its newline); refusing to replay".to_string(),
        ));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| PlaneClientError::CorruptJournal("journal is not utf-8".into()))?;
    let mut state = JournalState::default();
    let mut header_seen = false;
    let mut line_count: u64 = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        line_count += 1;
        let envelope: PlaneJournalLine = serde_json::from_str(line)
            .map_err(|e| PlaneClientError::CorruptJournal(format!("envelope parse: {e}")))?;
        if envelope.seq != state.next_seq {
            return Err(PlaneClientError::CorruptJournal(format!(
                "seq {} does not continue the chain (expected {}); a line may have been deleted",
                envelope.seq, state.next_seq
            )));
        }
        let expected = hmac_plane_line(key, envelope.seq, &state.tail_mac, &envelope.record);
        if envelope.mac != expected {
            return Err(PlaneClientError::CorruptJournal(
                "MAC mismatch (record tampered or a line deleted); refusing to replay".to_string(),
            ));
        }
        if line_count == 1 {
            // The first line is the identity header: the journal must
            // belong to THIS plane endpoint and root, or the backlog
            // belongs to a different configuration — fail closed rather
            // than letting another plane's decisions close it.
            let header: PlaneJournalHeader = serde_json::from_str(&envelope.record)
                .map_err(|e| PlaneClientError::CorruptJournal(format!("header parse: {e}")))?;
            if header.v != PLANE_RECORD_VERSION {
                return Err(PlaneClientError::CorruptJournal(format!(
                    "journal header version {} is not supported by this build (v{})",
                    header.v, PLANE_RECORD_VERSION
                )));
            }
            if header.plane != plane_identity || header.root_fp != root_fp {
                return Err(PlaneClientError::CorruptJournal(format!(
                    "journal belongs to plane {} (root {}), not {} (root {}); \
                     refusing to replay another configuration's backlog",
                    header.plane, header.root_fp, plane_identity, root_fp
                )));
            }
            header_seen = true;
            state.tail_mac = expected;
            state.next_seq += 1;
            continue;
        }
        let record: PlaneEventRecord = serde_json::from_str(&envelope.record)
            .map_err(|e| PlaneClientError::CorruptJournal(format!("record parse: {e}")))?;
        if record.v != PLANE_RECORD_VERSION {
            return Err(PlaneClientError::CorruptJournal(format!(
                "record version {} is not supported by this build (v{PLANE_RECORD_VERSION})",
                record.v
            )));
        }
        let event_id = record.event_id.clone();
        state.pending.retain(|r| r.event_id != event_id);
        state.deliver.retain(|r| r.event_id != event_id);
        match (&record.status, &record.outcome) {
            (PlaneEventStatus::Pending, _) => state.pending.push(record),
            (PlaneEventStatus::Deliver, _) => state.deliver.push(record),
            (PlaneEventStatus::Terminal, Some(PlaneOutcome::AmbiguousDelivery)) => {
                state.deliver.push(record);
            }
            // #544 review: a Terminal record whose outcome is STILL
            // non-definitive (e.g. Unreachable after a replay attempt
            // during an ongoing outage) must NOT close the event — it
            // stays in the deliver backlog so the advertised reconnect
            // replay can still attest it once the plane returns.
            (PlaneEventStatus::Terminal, Some(PlaneOutcome::Unreachable { .. })) => {
                state.deliver.push(record);
            }
            (PlaneEventStatus::Terminal, _) | (PlaneEventStatus::DupExplain, _) => {}
        }
        state.tail_mac = expected;
        state.next_seq += 1;
    }
    if line_count > 0 && !header_seen {
        return Err(PlaneClientError::CorruptJournal(
            "journal has records but no identity header; refusing to replay".to_string(),
        ));
    }
    // #544 review: a valid MAC-chained PREFIX is indistinguishable from a
    // fully-written journal whose tail records were deleted. Durably
    // checkpoint the observed byte length: on reopen, a shorter-but-valid
    // file (truncation of complete lines) fails closed instead of
    // resurrecting already-closed events for replay.
    let observed_len = bytes.len() as u64;
    let checkpoint_path = journal_checkpoint_path(path);
    let recorded_len: Option<u64> = std::fs::read_to_string(&checkpoint_path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    match recorded_len {
        Some(prev) if observed_len < prev => {
            return Err(PlaneClientError::CorruptJournal(format!(
                "journal shrank ({} bytes now, {} durably checkpointed); \
                 complete tail records were deleted",
                observed_len, prev
            )));
        }
        _ => {
            write_checkpoint(&checkpoint_path, observed_len)?;
        }
    }
    Ok(state)
}

/// The sidecar checkpoint path for a journal (`plane.jsonl.checkpoint`).
fn journal_checkpoint_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".checkpoint");
    path.with_file_name(name)
}

/// Durably record the journal length we verified (best-effort atomic
/// replace; a failure to checkpoint is not corruption — the next
/// successful open retries).
fn write_checkpoint(path: &std::path::PathBuf, len: u64) -> Result<(), PlaneClientError> {
    let tmp = path.with_extension("checkpoint.tmp");
    std::fs::write(&tmp, len.to_string())
        .and_then(|()| {
            std::fs::rename(&tmp, path).or_else(|_| {
                // rename across a same-dir tmp is atomic on our platforms;
                // a plain rewrite is the fallback.
                std::fs::write(path, len.to_string())
            })
        })
        .map_err(|e| PlaneClientError::Io(format!("checkpoint: {e}")))?;
    Ok(())
}

/// Append one record to the journal file (append-only, fsync'd) and advance
/// the chain state. Called under the journal lock.
fn append_locked(
    path: &Path,
    state: &mut JournalState,
    key: &[u8; 32],
    plane_identity: &str,
    root_fp: &str,
    record: &PlaneEventRecord,
) -> Result<(), PlaneClientError> {
    // First append writes the identity header line ahead of the record, so
    // every journal is self-describing (plane endpoint + root fingerprint,
    // MAC-chained like any other line).
    let mut out = String::new();
    if state.next_seq == 0 {
        let header = PlaneJournalHeader {
            v: PLANE_RECORD_VERSION,
            plane: plane_identity.to_string(),
            root_fp: root_fp.to_string(),
        };
        let hline = serde_json::to_string(&header)
            .map_err(|e| PlaneClientError::Io(format!("serialize header: {e}")))?;
        let hmac = hmac_plane_line(key, 0, &state.tail_mac, &hline);
        let henv = serde_json::json!({"mac": hmac, "record": hline, "seq": 0});
        out.push_str(
            &serde_json::to_string(&henv)
                .map_err(|e| PlaneClientError::Io(format!("envelope header: {e}")))?,
        );
        out.push('\n');
        state.next_seq = 1;
        state.tail_mac = hmac;
    }
    let line = serde_json::to_string(record)
        .map_err(|e| PlaneClientError::Io(format!("serialize: {e}")))?;
    let seq = state.next_seq;
    let mac = hmac_plane_line(key, seq, &state.tail_mac, &line);
    let envelope = serde_json::json!({"mac": mac, "record": line, "seq": seq});
    out.push_str(
        &serde_json::to_string(&envelope)
            .map_err(|e| PlaneClientError::Io(format!("envelope: {e}")))?,
    );
    out.push('\n');
    // Append-only: never truncate — the MAC chain makes any truncation
    // detectable at load, but producing one locally would be a self-inflicted
    // corruption.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| PlaneClientError::Io(format!("open for append: {e}")))?;
    file.write_all(out.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| PlaneClientError::Io(format!("append: {e}")))?;
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    state.next_seq += 1;
    state.tail_mac = mac;
    // Keep the durable length checkpoint in lockstep so a later
    // truncation of complete lines is detected at load.
    if let Ok(len) = std::fs::metadata(path).map(|m| m.len()) {
        let _ = write_checkpoint(&journal_checkpoint_path(path), len);
    }
    Ok(())
}

/// How a replayed send's answer is journaled, given the event's previously
/// durably-recorded outcome (if any) and whether a dispatch already
/// happened (a Deliver record, or a terminal response-unknown outcome —
/// the states that keep an event in the replay backlog).
///
/// - A re-send answered definitively is the EXPLAINED DUPLICATE: the
///   delivery may well have been duplicate (that is why the event was in
///   the backlog), and idempotent replay must be observable, not silent.
/// - A definitive answer that CONTRADICTS a previously recorded definitive
///   outcome is [`PlaneOutcome::CorruptEvidence`] — the plane changed its
///   mind outside the protocol, which must not silently pass. (Defense in
///   depth: the backlog is not normally built with a definitive outcome
///   present, but the journal is checked-not-trusted and a future status
///   must make an explicit decision here, not inherit one.)
/// - Anything else (a first delivery, a still-unknown answer) stands as-is.
fn replay_classification(
    previous: Option<&PlaneOutcome>,
    was_dispatched: bool,
    new: &PlaneOutcome,
) -> (PlaneEventStatus, PlaneOutcome) {
    let definitive = |o: &PlaneOutcome| {
        !matches!(
            o,
            PlaneOutcome::AmbiguousDelivery | PlaneOutcome::Unreachable { .. }
        )
    };
    match (previous, new) {
        (Some(prev), new) if definitive(prev) && definitive(new) && prev != new => (
            PlaneEventStatus::Terminal,
            PlaneOutcome::CorruptEvidence {
                detail: "replayed consult returned a conflicting decision".to_string(),
            },
        ),
        _ if was_dispatched && definitive(new) => (PlaneEventStatus::DupExplain, new.clone()),
        _ => (PlaneEventStatus::Terminal, new.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replayed_definitive_answer_is_the_explained_duplicate() {
        let (status, outcome) = replay_classification(None, true, &PlaneOutcome::Permitted);
        assert_eq!(status, PlaneEventStatus::DupExplain);
        assert_eq!(outcome, PlaneOutcome::Permitted);

        // An ambiguous prior (the response-unknown terminal) explained by a
        // definitive answer: still the explained duplicate.
        let (status, outcome) = replay_classification(
            Some(&PlaneOutcome::AmbiguousDelivery),
            true,
            &PlaneOutcome::Denied { reason: None },
        );
        assert_eq!(status, PlaneEventStatus::DupExplain);
        assert_eq!(outcome, PlaneOutcome::Denied { reason: None });
    }

    #[test]
    fn a_first_delivery_stands_as_terminal() {
        let (status, outcome) = replay_classification(None, false, &PlaneOutcome::Permitted);
        assert_eq!(status, PlaneEventStatus::Terminal);
        assert_eq!(outcome, PlaneOutcome::Permitted);
    }

    #[test]
    fn a_conflicting_definitive_re_decision_is_corrupt_evidence() {
        // Defense in depth: the journal is checked-not-trusted, and a
        // backlog record carrying a definitive prior must not be silently
        // re-decided by a contradicting answer.
        let (status, outcome) = replay_classification(
            Some(&PlaneOutcome::Permitted),
            true,
            &PlaneOutcome::Denied { reason: None },
        );
        assert_eq!(status, PlaneEventStatus::Terminal);
        assert!(matches!(outcome, PlaneOutcome::CorruptEvidence { .. }));
    }

    #[test]
    fn a_non_definitive_replay_answer_stands_as_terminal() {
        let (status, outcome) = replay_classification(
            Some(&PlaneOutcome::AmbiguousDelivery),
            true,
            &PlaneOutcome::AmbiguousDelivery,
        );
        assert_eq!(status, PlaneEventStatus::Terminal);
        assert_eq!(outcome, PlaneOutcome::AmbiguousDelivery);
    }
}
