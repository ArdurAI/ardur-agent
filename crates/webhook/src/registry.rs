use crate::error::WebhookError;
use crate::event::WebhookEvent;
use crate::inbound::{InboundWebhookHandler, InboundState, WebhookConfig};
use axum::routing::post;
use axum::Router;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::info;

/// Registered webhook endpoint metadata.
#[derive(Debug, Clone)]
pub struct WebhookEndpoint {
    /// The path this endpoint listens on (e.g., "/webhooks/slack").
    pub path: String,
    /// The source tag emitted on events from this endpoint.
    pub source: String,
    /// The config (secret, header name, etc.).
    pub config: WebhookConfig,
}

/// Registry that maps paths and event sources to handlers.
#[derive(Default)]
pub struct WebhookRegistry {
    endpoints: HashMap<String, WebhookEndpoint>,
    handlers: HashMap<String, Arc<dyn InboundWebhookHandler>>,
}

impl std::fmt::Debug for WebhookRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookRegistry")
            .field("endpoints", &self.endpoints)
            .field("handler_count", &self.handlers.len())
            .finish()
    }
}

impl WebhookRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an endpoint with its handler.
    pub fn register(
        &mut self,
        endpoint: WebhookEndpoint,
        handler: Arc<dyn InboundWebhookHandler>,
    ) {
        let source = endpoint.source.clone();
        let path = endpoint.path.clone();
        self.endpoints.insert(path.clone(), endpoint);
        self.handlers.insert(source, handler);
        info!("registered webhook endpoint {}", path);
    }

    /// Look up an endpoint by its path.
    pub fn endpoint_by_path(&self, path: &str) -> Option<&WebhookEndpoint> {
        self.endpoints.get(path)
    }

    /// Look up a handler by its source tag.
    pub fn handler_by_source(&self, source: &str) -> Option<&Arc<dyn InboundWebhookHandler>> {
        self.handlers.get(source)
    }

    /// Build an axum [`Router`] mounting all registered endpoints.
    ///
    /// Each endpoint is mounted as a `POST` handler at its configured path.
    pub fn router(&self) -> Router {
        let mut router = Router::new();
        for (path, endpoint) in &self.endpoints {
            let handler = self
                .handlers
                .get(&endpoint.source)
                .cloned()
                .unwrap_or_else(|| Arc::new(NoopHandler));
            let state = Arc::new(InboundState {
                config: endpoint.config.clone(),
                handler,
            });
            router = router.route(path, post(crate::inbound::receive_webhook).with_state(state));
        }
        router
    }
}

struct NoopHandler;

#[async_trait::async_trait]
impl InboundWebhookHandler for NoopHandler {
    async fn handle(&self, _event: WebhookEvent) -> Result<(), WebhookError> {
        Ok(())
    }
}
