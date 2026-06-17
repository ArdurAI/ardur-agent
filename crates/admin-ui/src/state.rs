//! [`AppState`] — the shared, read-only handle the HTTP handlers run against.

use std::path::PathBuf;
use std::sync::Arc;

use ardur_cap_token::VerifiedClaims;
use ardur_cedar_policy::CedarPolicyBundle;
use qdrant_client::Qdrant;

use crate::auth::BasicAuth;

/// A connected, read-only Qdrant source for the optional memory endpoint.
pub struct MemorySource {
    /// The Qdrant client (used only for `scroll` — a read).
    pub client: Qdrant,
    /// The collection memory records live in.
    pub collection: String,
}

impl std::fmt::Debug for MemorySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySource")
            .field("collection", &self.collection)
            .finish_non_exhaustive()
    }
}

/// Everything a handler needs, all of it read-only. Shared as `Arc<AppState>`
/// so the router can clone a cheap handle per request.
#[derive(Debug)]
pub struct AppState {
    /// Directory under which `sessions/<id>/journal.jsonl` live.
    pub journal_dir: PathBuf,
    /// Path to the receipt store (a directory holding `chain.jsonl`, or the
    /// `.jsonl` file itself).
    pub receipt_store: PathBuf,
    /// Connected Qdrant source, present only when `--qdrant-url` was given.
    pub memory: Option<MemorySource>,
    /// Required HTTP Basic credentials, if the operator configured a gate.
    pub basic_auth: Option<BasicAuth>,
    /// Verified active cap-token grants displayed by the capability wallet.
    pub capabilities: Vec<VerifiedClaims>,
    /// Cedar policy bundle used by the policy debugger.
    pub policies: Option<CedarPolicyBundle>,
}

impl AppState {
    /// Construct state for the filesystem artifacts, with no memory source and
    /// no auth gate (the defaults the tests build on).
    pub fn new(journal_dir: impl Into<PathBuf>, receipt_store: impl Into<PathBuf>) -> Self {
        Self {
            journal_dir: journal_dir.into(),
            receipt_store: receipt_store.into(),
            memory: None,
            basic_auth: None,
            capabilities: Vec::new(),
            policies: None,
        }
    }

    /// Attach a connected Qdrant memory source.
    #[must_use]
    pub fn with_memory(mut self, memory: MemorySource) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Require HTTP Basic auth on every endpoint.
    #[must_use]
    pub fn with_basic_auth(mut self, auth: BasicAuth) -> Self {
        self.basic_auth = Some(auth);
        self
    }

    /// Attach verified capability grants for the wallet view.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Vec<VerifiedClaims>) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Attach a Cedar policy bundle for policy-debugger traces.
    #[must_use]
    pub fn with_policies(mut self, policies: CedarPolicyBundle) -> Self {
        self.policies = Some(policies);
        self
    }

    /// Wrap into the shared handle the router is built from.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

/// The state type threaded through the axum router.
pub type SharedState = Arc<AppState>;
