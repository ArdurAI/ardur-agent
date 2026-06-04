//! `QdrantMemoryConfig::from_env` defaults + overrides.
//!
//! Lives in its own integration-test crate (not under the library's
//! `#![forbid(unsafe_code)]`) because `std::env::set_var`/`remove_var` are
//! `unsafe` under edition 2024. Run as a single `#[test]` so the process-global
//! `QDRANT_*` mutations stay sequential.

use ardur_memory_qdrant::QdrantMemoryConfig;

const KEYS: [&str; 5] = [
    "QDRANT_URL",
    "QDRANT_API_KEY",
    "QDRANT_COLLECTION",
    "QDRANT_VECTOR_DIM",
    "EMBED_MODEL",
];

fn clear() {
    for key in KEYS {
        unsafe { std::env::remove_var(key) };
    }
}

#[test]
fn from_env_defaults_then_overrides() {
    clear();
    // Unset → defaults.
    assert_eq!(
        QdrantMemoryConfig::from_env(),
        QdrantMemoryConfig::default()
    );

    unsafe {
        std::env::set_var("QDRANT_URL", "http://q:6334");
        std::env::set_var("QDRANT_API_KEY", "k");
        std::env::set_var("QDRANT_COLLECTION", "c");
        std::env::set_var("QDRANT_VECTOR_DIM", "768");
        std::env::set_var("EMBED_MODEL", "gte-base-en-v1.5");
    }
    let cfg = QdrantMemoryConfig::from_env();
    assert_eq!(cfg.url, "http://q:6334");
    assert_eq!(cfg.api_key.as_deref(), Some("k"));
    assert_eq!(cfg.collection_name, "c");
    assert_eq!(cfg.vector_dim, 768);
    assert_eq!(cfg.default_embed_model.as_deref(), Some("gte-base-en-v1.5"));

    // A malformed dim is ignored (default kept); an empty value reads as unset.
    unsafe {
        std::env::set_var("QDRANT_VECTOR_DIM", "not-a-number");
        std::env::set_var("QDRANT_API_KEY", "");
    }
    let cfg = QdrantMemoryConfig::from_env();
    assert_eq!(
        cfg.vector_dim, 384,
        "a malformed dim falls back to the default"
    );
    assert_eq!(cfg.api_key, None, "an empty api key reads as absent");

    clear();
}
