//! The operator webhook surface (§9.7).
//!
//! `WebhookOps` is the single gated facade over the outbound endpoint registry,
//! the inbound trigger registry, and the outbound emit path. Every action
//! admits through a cap-token scope check and emits a signed receipt; emits
//! also charge through the four-state receipt vocabulary
//! (`attempted` → `delivered` | `failed`). Endpoints and triggers are
//! owner-scoped: an operator only sees and mutates their own.

use chrono::Utc;
use secrecy::SecretString;
use uuid::Uuid;

use crate::endpoint::{
    DEFAULT_METHOD, DEFAULT_SIGNATURE_HEADER, EndpointRegistration, EndpointUpdate,
    OutboundEndpoint,
};
use crate::error::WebhookError;
use crate::gate::{
    Principal, ReceiptEvent, ReceiptSink, SCOPE_ENDPOINT_READ, SCOPE_ENDPOINT_REGISTER,
    SCOPE_INBOUND_REGISTER, SCOPE_OUTBOUND_EMIT,
};
use crate::opstore::JsonCollectionStore;
use crate::signature::sign_body;
use crate::trigger::{DEFAULT_REPLAY_WINDOW_SECS, InboundTrigger, TriggerRegistration};

/// The nonce header (replay defense at the receiver).
pub const NONCE_HEADER: &str = "X-Ardur-Emit-Nonce";
/// The idempotency-key header.
pub const IDEMPOTENCY_HEADER: &str = "X-Ardur-Idempotency-Key";

/// A request to POST/PUT/etc a signed body to an endpoint.
#[derive(Debug, Clone)]
pub struct DispatchRequest {
    /// Destination URL.
    pub url: String,
    /// HTTP method.
    pub method: String,
    /// Signed body bytes.
    pub body: Vec<u8>,
    /// Headers (signature, nonce, idempotency, content-type).
    pub headers: Vec<(String, String)>,
}

/// The outcome of a dispatch.
#[derive(Debug, Clone)]
pub struct DispatchResult {
    /// Receiver HTTP status.
    pub status: u16,
}

/// The transport used to actually send an emit. Kept abstract so the crate is
/// network-free and testable; the CLI wires a real HTTP dispatcher.
pub trait Dispatcher {
    /// Send the request; return the receiver status, or a transport error.
    fn dispatch(&self, request: &DispatchRequest) -> Result<DispatchResult, WebhookError>;
}

/// The result of an emit, carrying the receipt evidence.
#[derive(Debug, Clone)]
pub struct EmitReport {
    /// The emit id.
    pub emit_id: String,
    /// The endpoint emitted to.
    pub endpoint_id: String,
    /// Receiver status, when the transport completed.
    pub status: Option<u16>,
    /// Whether the receiver returned 2xx.
    pub delivered: bool,
    /// Receipt id of the terminal (delivered | failed) receipt.
    pub receipt_id: Option<String>,
}

/// The gated operator webhook facade.
pub struct WebhookOps<R: ReceiptSink> {
    endpoints: JsonCollectionStore<OutboundEndpoint>,
    triggers: JsonCollectionStore<InboundTrigger>,
    receipts: R,
}

impl<R: ReceiptSink> WebhookOps<R> {
    /// Build the facade over the two registries + a receipt sink.
    pub fn new(
        endpoints: JsonCollectionStore<OutboundEndpoint>,
        triggers: JsonCollectionStore<InboundTrigger>,
        receipts: R,
    ) -> Self {
        Self {
            endpoints,
            triggers,
            receipts,
        }
    }

    /// Borrow the receipt sink (for wiring / tests).
    pub fn receipt_sink(&self) -> &R {
        &self.receipts
    }

    fn receipt(
        &self,
        verb: &str,
        principal: &Principal,
        payload: &[u8],
    ) -> Result<String, WebhookError> {
        self.receipts.emit(ReceiptEvent {
            verb,
            subject: &principal.subject,
            token_id: &principal.token_id,
            payload,
        })
    }

    // --- Outbound endpoint registry -------------------------------------

    /// Register a new outbound endpoint. Requires
    /// [`SCOPE_ENDPOINT_REGISTER`]. Emits `webhook.endpoint.registered.v1`.
    pub fn register_endpoint(
        &self,
        principal: &Principal,
        reg: EndpointRegistration,
    ) -> Result<String, WebhookError> {
        principal.require(SCOPE_ENDPOINT_REGISTER)?;
        if reg.url.trim().is_empty() {
            return Err(WebhookError::InvalidEndpoint("empty url".to_string()));
        }
        if reg.secret_env.trim().is_empty() {
            return Err(WebhookError::InvalidEndpoint(
                "secret_env must name the environment variable holding the HMAC secret".to_string(),
            ));
        }
        let now = Utc::now();
        let id = Uuid::now_v7().to_string();
        let endpoint = OutboundEndpoint {
            id: id.clone(),
            name: reg.name,
            url: reg.url,
            method: reg.method.unwrap_or_else(|| DEFAULT_METHOD.to_string()),
            secret_env: reg.secret_env,
            signature_header: reg
                .signature_header
                .unwrap_or_else(|| DEFAULT_SIGNATURE_HEADER.to_string()),
            owner_fingerprint: principal.fingerprint.clone(),
            registered_at: now,
            updated_at: now,
            revoked: false,
            last_status: None,
            last_attempt_at: None,
        };
        self.endpoints.upsert(endpoint)?;
        self.receipt("webhook.endpoint.registered.v1", principal, id.as_bytes())?;
        Ok(id)
    }

    /// List endpoints owned by the operator. Requires [`SCOPE_ENDPOINT_READ`].
    pub fn list_endpoints(
        &self,
        principal: &Principal,
    ) -> Result<Vec<OutboundEndpoint>, WebhookError> {
        principal.require(SCOPE_ENDPOINT_READ)?;
        let all = self.endpoints.load_all()?;
        let owned: Vec<OutboundEndpoint> = all
            .into_iter()
            .filter(|e| e.owner_fingerprint == principal.fingerprint)
            .collect();
        self.receipt(
            "webhook.endpoint.listed.v1",
            principal,
            format!("{}", owned.len()).as_bytes(),
        )?;
        Ok(owned)
    }

    /// Fetch one owned endpoint by id.
    pub fn get_endpoint(
        &self,
        principal: &Principal,
        id: &str,
    ) -> Result<OutboundEndpoint, WebhookError> {
        principal.require(SCOPE_ENDPOINT_READ)?;
        self.owned_endpoint(principal, id)
    }

    fn owned_endpoint(
        &self,
        principal: &Principal,
        id: &str,
    ) -> Result<OutboundEndpoint, WebhookError> {
        let endpoint = self.endpoints.get(id)?;
        if endpoint.owner_fingerprint != principal.fingerprint {
            return Err(WebhookError::Denied(format!(
                "endpoint `{id}` not owned by operator"
            )));
        }
        Ok(endpoint)
    }

    /// Update an endpoint's mutable fields. Requires
    /// [`SCOPE_ENDPOINT_REGISTER`]. Emits `webhook.endpoint.updated.v1`.
    pub fn update_endpoint(
        &self,
        principal: &Principal,
        id: &str,
        update: EndpointUpdate,
    ) -> Result<(), WebhookError> {
        principal.require(SCOPE_ENDPOINT_REGISTER)?;
        let mut endpoint = self.owned_endpoint(principal, id)?;
        if let Some(url) = update.url {
            endpoint.url = url;
        }
        if let Some(method) = update.method {
            endpoint.method = method;
        }
        if let Some(secret_env) = update.secret_env {
            endpoint.secret_env = secret_env;
        }
        if let Some(header) = update.signature_header {
            endpoint.signature_header = header;
        }
        endpoint.updated_at = Utc::now();
        self.endpoints.upsert(endpoint)?;
        self.receipt("webhook.endpoint.updated.v1", principal, id.as_bytes())?;
        Ok(())
    }

    /// Revoke (soft-delete) an endpoint. Requires [`SCOPE_ENDPOINT_REGISTER`].
    /// Emits `webhook.endpoint.revoked.v1`.
    pub fn revoke_endpoint(&self, principal: &Principal, id: &str) -> Result<(), WebhookError> {
        principal.require(SCOPE_ENDPOINT_REGISTER)?;
        let mut endpoint = self.owned_endpoint(principal, id)?;
        endpoint.revoked = true;
        endpoint.updated_at = Utc::now();
        self.endpoints.upsert(endpoint)?;
        self.receipt("webhook.endpoint.revoked.v1", principal, id.as_bytes())?;
        Ok(())
    }

    // --- Outbound emit ---------------------------------------------------

    /// Sign and dispatch a payload to a registered endpoint. Requires
    /// [`SCOPE_OUTBOUND_EMIT`]. Emits `webhook.outbound.attempted.v1` then a
    /// terminal `webhook.outbound.delivered.v1` (2xx) or
    /// `webhook.outbound.failed.v1`.
    pub fn emit(
        &self,
        principal: &Principal,
        endpoint_id: &str,
        payload: &[u8],
        dispatcher: &dyn Dispatcher,
    ) -> Result<EmitReport, WebhookError> {
        principal.require(SCOPE_OUTBOUND_EMIT)?;
        let endpoint = self.owned_endpoint(principal, endpoint_id)?;
        if endpoint.revoked {
            return Err(WebhookError::Denied(format!(
                "endpoint `{endpoint_id}` is revoked"
            )));
        }

        let emit_id = Uuid::now_v7().to_string();
        self.receipt(
            "webhook.outbound.attempted.v1",
            principal,
            emit_id.as_bytes(),
        )?;

        // Resolve the HMAC secret from its environment ref — never stored.
        let secret_value = std::env::var(&endpoint.secret_env).map_err(|_| {
            WebhookError::SigningKeyResolveFailed(format!(
                "environment variable `{}` is not set",
                endpoint.secret_env
            ))
        })?;
        let secret = SecretString::new(secret_value.into());
        let signature = sign_body(payload, &secret)?;
        let nonce = Uuid::now_v7().to_string();
        let idempotency_key = format!("{}:{}", principal.token_id, emit_id);
        let headers = vec![
            (
                endpoint.signature_header.clone(),
                format!("sha256={signature}"),
            ),
            (NONCE_HEADER.to_string(), nonce),
            (IDEMPOTENCY_HEADER.to_string(), idempotency_key),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        let request = DispatchRequest {
            url: endpoint.url.clone(),
            method: endpoint.method.clone(),
            body: payload.to_vec(),
            headers,
        };

        let outcome = dispatcher.dispatch(&request);
        let now = Utc::now();
        let (status, delivered, receipt_id) = match outcome {
            Ok(result) => {
                let delivered = (200..300).contains(&result.status);
                let verb = if delivered {
                    "webhook.outbound.delivered.v1"
                } else {
                    "webhook.outbound.failed.v1"
                };
                let rid = self.receipt(verb, principal, emit_id.as_bytes())?;
                (Some(result.status), delivered, Some(rid))
            }
            Err(err) => {
                let rid = self.receipt(
                    "webhook.outbound.failed.v1",
                    principal,
                    format!("{emit_id}:{err}").as_bytes(),
                )?;
                (None, false, Some(rid))
            }
        };

        // Best-effort update of the endpoint's last-emit metadata.
        let mut updated = endpoint;
        updated.last_status = status;
        updated.last_attempt_at = Some(now);
        let _ = self.endpoints.upsert(updated);

        Ok(EmitReport {
            emit_id,
            endpoint_id: endpoint_id.to_string(),
            status,
            delivered,
            receipt_id,
        })
    }

    // --- Inbound trigger registry ---------------------------------------

    /// Register a new inbound trigger. Requires [`SCOPE_INBOUND_REGISTER`].
    /// Emits `webhook.inbound.registered.v1`.
    pub fn register_trigger(
        &self,
        principal: &Principal,
        reg: TriggerRegistration,
    ) -> Result<String, WebhookError> {
        principal.require(SCOPE_INBOUND_REGISTER)?;
        if reg.path.trim().is_empty() || !reg.path.starts_with('/') {
            return Err(WebhookError::InvalidEndpoint(
                "trigger path must be an absolute route (start with `/`)".to_string(),
            ));
        }
        if reg.secret_env.trim().is_empty() {
            return Err(WebhookError::InvalidEndpoint(
                "secret_env must name the environment variable holding the HMAC secret".to_string(),
            ));
        }
        let now = Utc::now();
        let id = Uuid::now_v7().to_string();
        let trigger = InboundTrigger {
            id: id.clone(),
            name: reg.name,
            path: reg.path,
            source: reg.source,
            secret_env: reg.secret_env,
            action: reg.action,
            replay_window_secs: reg.replay_window_secs.unwrap_or(DEFAULT_REPLAY_WINDOW_SECS),
            owner_fingerprint: principal.fingerprint.clone(),
            registered_at: now,
            updated_at: now,
            enabled: true,
        };
        self.triggers.upsert(trigger)?;
        self.receipt("webhook.inbound.registered.v1", principal, id.as_bytes())?;
        Ok(id)
    }

    /// List inbound triggers owned by the operator. Requires
    /// [`SCOPE_ENDPOINT_READ`].
    pub fn list_triggers(
        &self,
        principal: &Principal,
    ) -> Result<Vec<InboundTrigger>, WebhookError> {
        principal.require(SCOPE_ENDPOINT_READ)?;
        let owned: Vec<InboundTrigger> = self
            .triggers
            .load_all()?
            .into_iter()
            .filter(|t| t.owner_fingerprint == principal.fingerprint)
            .collect();
        self.receipt(
            "webhook.inbound.listed.v1",
            principal,
            format!("{}", owned.len()).as_bytes(),
        )?;
        Ok(owned)
    }

    /// Remove an inbound trigger. Requires [`SCOPE_INBOUND_REGISTER`]. Emits
    /// `webhook.inbound.removed.v1`.
    pub fn remove_trigger(&self, principal: &Principal, id: &str) -> Result<(), WebhookError> {
        principal.require(SCOPE_INBOUND_REGISTER)?;
        let trigger = self.triggers.get(id)?;
        if trigger.owner_fingerprint != principal.fingerprint {
            return Err(WebhookError::Denied(format!(
                "trigger `{id}` not owned by operator"
            )));
        }
        self.triggers.remove(id)?;
        self.receipt("webhook.inbound.removed.v1", principal, id.as_bytes())?;
        Ok(())
    }
}
