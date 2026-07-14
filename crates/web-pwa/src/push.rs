//! Builds and sends one Web Push notification: VAPID-sign, RFC 8188
//! `aes128gcm`-encrypt the payload (via [`web_push`]), then POST it directly
//! with the workspace's `reqwest` client rather than `web_push`'s own bundled
//! client — see the crate docs' adapt-points note on why.

use web_push::{
    ContentEncoding, SubscriptionInfo, SubscriptionKeys, VapidSignatureBuilder,
    WebPushMessageBuilder,
};

use crate::error::PwaError;
use crate::keys::VapidKeyPair;
use crate::subscription::PushSubscription;

/// The JSON payload the service worker's `push` handler expects — matching
/// the shape `web-client/sw.js` already parses (`title`/`body`/`approval_id`
/// or a generic `url`).
#[derive(serde::Serialize)]
pub struct PushPayload<'a> {
    /// Notification title.
    pub title: &'a str,
    /// Notification body text.
    pub body: &'a str,
    /// Deep-link URL the click handler opens.
    pub url: &'a str,
}

/// VAPID-sign, encrypt, and deliver `payload` to `subscription`.
///
/// # Errors
/// [`PwaError::MessageBuild`] if the VAPID signature or payload encryption
/// fails; [`PwaError::DeliveryFailed`] if the push service rejects the
/// request or the request cannot be sent.
pub async fn send(
    http: &reqwest::Client,
    vapid: &VapidKeyPair,
    subscription: &PushSubscription,
    payload: &PushPayload<'_>,
) -> Result<(), PwaError> {
    let info = SubscriptionInfo {
        endpoint: subscription.endpoint.clone(),
        keys: SubscriptionKeys {
            p256dh: subscription.p256dh.clone(),
            auth: subscription.auth.clone(),
        },
    };

    let pem = vapid
        .pkcs8_pem()
        .map_err(|e| PwaError::MessageBuild(e.to_string()))?;
    let sig_builder = VapidSignatureBuilder::from_pem(pem.as_bytes(), &info)
        .map_err(|e| PwaError::MessageBuild(e.to_string()))?
        .build()
        .map_err(|e| PwaError::MessageBuild(e.to_string()))?;

    let body = serde_json::to_vec(payload).map_err(|e| PwaError::MessageBuild(e.to_string()))?;

    let mut builder = WebPushMessageBuilder::new(&info);
    builder.set_payload(ContentEncoding::Aes128Gcm, &body);
    builder.set_vapid_signature(sig_builder);
    let message = builder
        .build()
        .map_err(|e| PwaError::MessageBuild(e.to_string()))?;

    // `message.endpoint` is a `http` 0.2 `Uri` (`web_push`'s pinned version);
    // re-parse as a `reqwest::Url` rather than pulling `http` 0.2 into this
    // crate's own dependency surface.
    let url = reqwest::Url::parse(&message.endpoint.to_string())
        .map_err(|e| PwaError::MessageBuild(format!("endpoint is not a valid URL: {e}")))?;

    let mut request = http.post(url).header("TTL", message.ttl.to_string());
    if let Some(urgency) = message.urgency {
        request = request.header("Urgency", urgency.to_string());
    }
    if let Some(topic) = message.topic {
        request = request.header("Topic", topic);
    }
    if let Some(push_payload) = message.payload {
        request = request
            .header("Content-Encoding", push_payload.content_encoding.to_str())
            .header("Content-Type", "application/octet-stream");
        for (name, value) in push_payload.crypto_headers {
            request = request.header(name, value);
        }
        request = request.body(push_payload.content);
    }

    let response = request
        .send()
        .await
        .map_err(|e| PwaError::DeliveryFailed(e.to_string()))?;

    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(PwaError::DeliveryFailed(format!("{status}: {body}")))
    }
}
