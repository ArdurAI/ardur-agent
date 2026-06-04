//! [`QdrantMemoryConfig`] — how to reach the Qdrant instance and how the
//! `ardur_memory` collection is shaped.
//!
//! Every knob has a default tuned for a local Qdrant (the gRPC port `6334`, an
//! `ardur_memory` collection, a 384-d vector space) and an environment override.
//! [`QdrantMemoryConfig::from_env`] is the production path; the builder methods
//! (`with_*`) back the tests and any programmatic construction.

/// Connection + collection settings for the Qdrant-backed memory runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QdrantMemoryConfig {
    /// The Qdrant gRPC endpoint (`QDRANT_URL`, default `http://localhost:6334`).
    pub url: String,
    /// API key for a secured (e.g. Qdrant Cloud) instance (`QDRANT_API_KEY`);
    /// `None` for an unauthenticated local instance.
    pub api_key: Option<String>,
    /// The collection bi-temporal records are stored in (`QDRANT_COLLECTION`,
    /// default `ardur_memory`).
    pub collection_name: String,
    /// The dimensionality of the stored vectors (`QDRANT_VECTOR_DIM`, default
    /// `384`). Must match the embedding model once real embeddings land; see the
    /// `// TODO §7.0 Phase 2` placeholder embedding in `runtime.rs`.
    pub vector_dim: usize,
}

impl Default for QdrantMemoryConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:6334".to_string(),
            api_key: None,
            collection_name: "ardur_memory".to_string(),
            vector_dim: 384,
        }
    }
}

impl QdrantMemoryConfig {
    /// A config with every field at its default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the config from the process environment, falling back to the
    /// [`Default`] for any unset (or empty) variable. A malformed
    /// `QDRANT_VECTOR_DIM` is ignored (the default dim is kept).
    #[must_use]
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Some(url) = non_empty("QDRANT_URL") {
            cfg.url = url;
        }
        cfg.api_key = non_empty("QDRANT_API_KEY");
        if let Some(collection) = non_empty("QDRANT_COLLECTION") {
            cfg.collection_name = collection;
        }
        if let Some(dim) = non_empty("QDRANT_VECTOR_DIM").and_then(|v| v.parse().ok()) {
            cfg.vector_dim = dim;
        }
        cfg
    }

    /// Override the Qdrant endpoint URL.
    #[must_use]
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// Set the API key (for a secured instance).
    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Override the collection name.
    #[must_use]
    pub fn with_collection_name(mut self, name: impl Into<String>) -> Self {
        self.collection_name = name.into();
        self
    }

    /// Override the vector dimensionality.
    #[must_use]
    pub fn with_vector_dim(mut self, dim: usize) -> Self {
        self.vector_dim = dim;
        self
    }
}

/// Read an environment variable, treating an unset *or* empty value as absent.
fn non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_local_qdrant() {
        let cfg = QdrantMemoryConfig::default();
        assert_eq!(cfg.url, "http://localhost:6334");
        assert_eq!(cfg.api_key, None);
        assert_eq!(cfg.collection_name, "ardur_memory");
        assert_eq!(cfg.vector_dim, 384);
        // `new()` is the same as `default()`.
        assert_eq!(QdrantMemoryConfig::new(), cfg);
    }

    #[test]
    fn builder_overrides_each_field() {
        let cfg = QdrantMemoryConfig::new()
            .with_url("http://qdrant.internal:6334")
            .with_api_key("secret")
            .with_collection_name("custom")
            .with_vector_dim(1024);
        assert_eq!(cfg.url, "http://qdrant.internal:6334");
        assert_eq!(cfg.api_key.as_deref(), Some("secret"));
        assert_eq!(cfg.collection_name, "custom");
        assert_eq!(cfg.vector_dim, 1024);
    }

    // `from_env`'s defaults-then-overrides behaviour is exercised in
    // `tests/config_env.rs`: setting env vars is `unsafe` under edition 2024,
    // which this crate's `#![forbid(unsafe_code)]` rejects inside the library, so
    // the env-mutating test lives in a separate integration-test crate.
}
