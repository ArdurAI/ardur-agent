//! [`PwaService`] — the wired state behind [`router`] — and the axum routes
//! themselves: `GET /vapid-public-key`, `POST /subscribe`,
//! `POST /unsubscribe`, `POST /test`.

use std::path::Path;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::PwaError;
use crate::keys::VapidKeyPair;
use crate::push::{self, PushPayload};
use crate::subscription::{PushSubscription, SubscriptionStore};

/// The wired PWA state: the VAPID identity key, the subscription store, and
/// the HTTP client push sends go out over.
pub struct PwaService {
    vapid: VapidKeyPair,
    subscriptions: SubscriptionStore,
    http: reqwest::Client,
}

impl PwaService {
    /// Load (or generate) the VAPID key and subscription table under
    /// `data_dir` (`<data_dir>/pwa/vapid.pem` and
    /// `<data_dir>/pwa/subscriptions.json`).
    ///
    /// # Errors
    /// [`PwaError::KeyLoad`] / [`PwaError::Persist`] per
    /// [`VapidKeyPair::load_or_generate`] / [`SubscriptionStore::load`].
    pub async fn load(data_dir: &Path) -> Result<Self, PwaError> {
        let pwa_dir = data_dir.join("pwa");
        let vapid = VapidKeyPair::load_or_generate(&pwa_dir.join("vapid.pem")).await?;
        let subscriptions = SubscriptionStore::load(pwa_dir.join("subscriptions.json")).await?;
        Ok(Self {
            vapid,
            subscriptions,
            http: reqwest::Client::new(),
        })
    }

    /// The VAPID public key, base64url-encoded for
    /// `PushManager.subscribe({ applicationServerKey })`.
    #[must_use]
    pub fn vapid_public_key(&self) -> String {
        self.vapid.public_key_b64url()
    }

    /// Validate and register a subscription, returning its assigned id.
    ///
    /// # Errors
    /// As [`SubscriptionStore::register`].
    pub async fn subscribe(&self, subscription: PushSubscription) -> Result<Uuid, PwaError> {
        self.subscriptions.register(subscription).await
    }

    /// Remove a subscription. Idempotent — an unknown id is not an error.
    ///
    /// # Errors
    /// As [`SubscriptionStore::unregister`].
    pub async fn unsubscribe(&self, id: Uuid) -> Result<(), PwaError> {
        self.subscriptions.unregister(id).await
    }

    /// VAPID-sign, encrypt, and deliver `payload` to the subscription
    /// registered under `id`. Exposed as a Rust API for other in-process
    /// callers — e.g. a future "notify me when my run finishes" hook — not
    /// as an HTTP route (see the crate docs' adapt-points note on why an
    /// open send endpoint is out of scope for Phase 1).
    ///
    /// # Errors
    /// [`PwaError::UnknownSubscription`] if `id` is not registered; otherwise
    /// as [`push::send`].
    pub async fn send(&self, id: Uuid, payload: &PushPayload<'_>) -> Result<(), PwaError> {
        let subscription = self
            .subscriptions
            .get(id)
            .ok_or(PwaError::UnknownSubscription(id))?;
        push::send(&self.http, &self.vapid, &subscription, payload).await
    }

    /// Send a canned "it works" notification to `id` — the one HTTP-exposed
    /// send path, scoped to the caller's own just-registered subscription
    /// rather than an arbitrary target.
    async fn send_test(&self, id: Uuid) -> Result<(), PwaError> {
        self.send(
            id,
            &PushPayload {
                title: "Ardur push test",
                body: "If you can see this, web push is working.",
                url: "./index.html",
            },
        )
        .await
    }
}

impl IntoResponse for PwaError {
    fn into_response(self) -> Response {
        let status = match &self {
            PwaError::InvalidSubscription(_) => StatusCode::BAD_REQUEST,
            PwaError::UnknownSubscription(_) => StatusCode::NOT_FOUND,
            PwaError::KeyLoad(_) | PwaError::Persist(_) | PwaError::MessageBuild(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            PwaError::DeliveryFailed(_) => StatusCode::BAD_GATEWAY,
        };
        (status, self.to_string()).into_response()
    }
}

#[derive(Serialize, Deserialize)]
struct VapidPublicKeyResponse {
    #[serde(rename = "publicKey")]
    public_key: String,
}

async fn vapid_public_key(State(service): State<Arc<PwaService>>) -> Json<VapidPublicKeyResponse> {
    Json(VapidPublicKeyResponse {
        public_key: service.vapid_public_key(),
    })
}

#[derive(Serialize, Deserialize)]
struct SubscribeResponse {
    id: Uuid,
}

async fn subscribe(
    State(service): State<Arc<PwaService>>,
    Json(subscription): Json<PushSubscription>,
) -> Result<Json<SubscribeResponse>, PwaError> {
    let id = service.subscribe(subscription).await?;
    Ok(Json(SubscribeResponse { id }))
}

#[derive(Serialize, Deserialize)]
struct SubscriptionIdRequest {
    id: Uuid,
}

async fn unsubscribe(
    State(service): State<Arc<PwaService>>,
    Json(req): Json<SubscriptionIdRequest>,
) -> Result<StatusCode, PwaError> {
    service.unsubscribe(req.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn test(
    State(service): State<Arc<PwaService>>,
    Json(req): Json<SubscriptionIdRequest>,
) -> Result<StatusCode, PwaError> {
    service.send_test(req.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The PWA HTTP surface: `GET /vapid-public-key`, `POST /subscribe`,
/// `POST /unsubscribe`, `POST /test`. The server mounts this under an opt-in
/// path prefix (e.g. `.nest("/pwa", ardur_web_pwa::router(service))`), gated
/// on a boot-time config flag like the channel adapters.
pub fn router(service: Arc<PwaService>) -> Router {
    Router::new()
        .route("/vapid-public-key", get(vapid_public_key))
        .route("/subscribe", post(subscribe))
        .route("/unsubscribe", post(unsubscribe))
        .route("/test", post(test))
        .with_state(service)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn test_service() -> Arc<PwaService> {
        let dir = tempfile::tempdir().expect("tempdir");
        Arc::new(
            PwaService::load(dir.path())
                .await
                .expect("loads a fresh service"),
        )
    }

    #[tokio::test]
    async fn vapid_public_key_route_returns_the_key() {
        let service = test_service().await;
        let expected = service.vapid_public_key();
        let app = router(service);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/vapid-public-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: VapidPublicKeyResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.public_key, expected);
    }

    #[tokio::test]
    async fn subscribe_then_unsubscribe_round_trips() {
        let service = test_service().await;
        let app = router(service.clone());

        let sub = PushSubscription {
            endpoint: "https://push.example.com/x".to_owned(),
            p256dh: "key".to_owned(),
            auth: "secret".to_owned(),
        };
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/subscribe")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&sub).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: SubscribeResponse = serde_json::from_slice(&body).unwrap();
        assert!(service.subscriptions.get(parsed.id).is_some());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/unsubscribe")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&SubscriptionIdRequest { id: parsed.id }).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(service.subscriptions.get(parsed.id).is_none());
    }

    #[tokio::test]
    async fn subscribe_rejects_invalid_endpoint() {
        let service = test_service().await;
        let app = router(service);

        let sub = PushSubscription {
            endpoint: "http://not-https.example.com/x".to_owned(),
            p256dh: "key".to_owned(),
            auth: "secret".to_owned(),
        };
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/subscribe")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&sub).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_route_404s_on_unknown_subscription() {
        let service = test_service().await;
        let app = router(service);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/test")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&SubscriptionIdRequest { id: Uuid::new_v4() }).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
