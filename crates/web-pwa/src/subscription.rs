//! [`PushSubscription`] and the [`SubscriptionStore`] that persists them.
//!
//! Phase 1 persists subscriptions as a flat JSON file rather than the
//! blueprint's SessionStore-projected rows (`plans/10.14-pwa-installable-
//! web-client-blueprint.md`'s "Ardur unique angle" section) — see the crate
//! docs' adapt-points note. The store is still the single point every
//! subscribe/unsubscribe/send call goes through, so upgrading the backing
//! store later does not change this crate's public API.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::PwaError;

/// A push endpoint's `endpoint`/`p256dh`/`auth` triple, as delivered by
/// `PushManager.subscribe()` in the browser.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushSubscription {
    /// The push service's per-subscription delivery URL.
    pub endpoint: String,
    /// The subscriber's P-256 Diffie-Hellman public key (base64url).
    pub p256dh: String,
    /// The subscriber's authentication secret (base64url).
    pub auth: String,
}

/// Endpoint validation limits, per the OpenClaw precedent cited in the
/// blueprint (`src/infra/push-web.ts:71-83`): HTTPS only, bounded length.
const MAX_ENDPOINT_LEN: usize = 2048;

impl PushSubscription {
    /// Validate the subscription's shape.
    ///
    /// # Errors
    /// [`PwaError::InvalidSubscription`] if `endpoint` is not `https://`, is
    /// longer than 2048 chars, or either key field is empty.
    pub fn validate(&self) -> Result<(), PwaError> {
        if !self.endpoint.starts_with("https://") {
            return Err(PwaError::InvalidSubscription(
                "endpoint must be https://".to_owned(),
            ));
        }
        if self.endpoint.len() > MAX_ENDPOINT_LEN {
            return Err(PwaError::InvalidSubscription(format!(
                "endpoint exceeds {MAX_ENDPOINT_LEN} chars"
            )));
        }
        if self.p256dh.is_empty() || self.auth.is_empty() {
            return Err(PwaError::InvalidSubscription(
                "p256dh and auth must be non-empty".to_owned(),
            ));
        }
        Ok(())
    }
}

/// An in-memory subscription table, mirrored to a JSON file on every mutation
/// so it survives a restart.
pub struct SubscriptionStore {
    path: PathBuf,
    subscriptions: Mutex<HashMap<Uuid, PushSubscription>>,
}

impl SubscriptionStore {
    /// Load the store from `path` if it exists (an empty table otherwise).
    /// The file is created on the first [`register`](Self::register) call.
    ///
    /// # Errors
    /// [`PwaError::Persist`] if the file exists but fails to parse.
    pub async fn load(path: PathBuf) -> Result<Self, PwaError> {
        let subscriptions = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => serde_json::from_str(&raw).map_err(|e| PwaError::Persist(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(PwaError::Persist(e.to_string())),
        };
        Ok(Self {
            path,
            subscriptions: Mutex::new(subscriptions),
        })
    }

    /// Validate and register a subscription, returning its assigned id.
    ///
    /// # Errors
    /// [`PwaError::InvalidSubscription`] per [`PushSubscription::validate`];
    /// [`PwaError::Persist`] if the updated table cannot be written to disk.
    pub async fn register(&self, subscription: PushSubscription) -> Result<Uuid, PwaError> {
        subscription.validate()?;
        let id = Uuid::new_v4();
        {
            let mut table = self
                .subscriptions
                .lock()
                .expect("subscriptions mutex poisoned");
            table.insert(id, subscription);
        }
        self.persist().await?;
        Ok(id)
    }

    /// Remove a subscription by id. A missing id is not an error (repeated
    /// unsubscribe calls are idempotent).
    ///
    /// # Errors
    /// [`PwaError::Persist`] if the updated table cannot be written to disk.
    pub async fn unregister(&self, id: Uuid) -> Result<(), PwaError> {
        {
            let mut table = self
                .subscriptions
                .lock()
                .expect("subscriptions mutex poisoned");
            table.remove(&id);
        }
        self.persist().await
    }

    /// Look up a subscription by id.
    #[must_use]
    pub fn get(&self, id: Uuid) -> Option<PushSubscription> {
        self.subscriptions
            .lock()
            .expect("subscriptions mutex poisoned")
            .get(&id)
            .cloned()
    }

    /// Write the current table to disk (parent directories created as
    /// needed).
    async fn persist(&self) -> Result<(), PwaError> {
        let snapshot = {
            let table = self
                .subscriptions
                .lock()
                .expect("subscriptions mutex poisoned");
            table.clone()
        };
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| PwaError::Persist(e.to_string()))?;
        }
        let json =
            serde_json::to_vec_pretty(&snapshot).map_err(|e| PwaError::Persist(e.to_string()))?;
        tokio::fs::write(&self.path, json)
            .await
            .map_err(|e| PwaError::Persist(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(endpoint: &str) -> PushSubscription {
        PushSubscription {
            endpoint: endpoint.to_owned(),
            p256dh: "key".to_owned(),
            auth: "secret".to_owned(),
        }
    }

    #[test]
    fn validate_rejects_non_https_endpoint() {
        let err = sample("http://push.example.com/x").validate().unwrap_err();
        assert!(matches!(err, PwaError::InvalidSubscription(_)));
    }

    #[test]
    fn validate_rejects_oversized_endpoint() {
        let long = format!("https://push.example.com/{}", "x".repeat(MAX_ENDPOINT_LEN));
        assert!(sample(&long).validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_keys() {
        let mut sub = sample("https://push.example.com/x");
        sub.p256dh = String::new();
        assert!(sub.validate().is_err());
    }

    #[tokio::test]
    async fn register_persists_and_reloads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("subscriptions.json");

        let store = SubscriptionStore::load(path.clone())
            .await
            .expect("loads empty store");
        let id = store
            .register(sample("https://push.example.com/a"))
            .await
            .expect("registers valid subscription");
        assert!(store.get(id).is_some());

        let reloaded = SubscriptionStore::load(path)
            .await
            .expect("reloads persisted store");
        assert_eq!(
            reloaded.get(id),
            Some(sample("https://push.example.com/a")),
            "the subscription survives a reload"
        );
    }

    #[tokio::test]
    async fn register_rejects_invalid_subscription_without_persisting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("subscriptions.json");
        let store = SubscriptionStore::load(path).await.expect("loads");

        let err = store
            .register(sample("http://not-https.example.com/a"))
            .await
            .unwrap_err();
        assert!(matches!(err, PwaError::InvalidSubscription(_)));
    }

    #[tokio::test]
    async fn unregister_missing_id_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("subscriptions.json");
        let store = SubscriptionStore::load(path).await.expect("loads");
        store
            .unregister(Uuid::new_v4())
            .await
            .expect("unregistering an unknown id is a no-op, not an error");
    }
}
