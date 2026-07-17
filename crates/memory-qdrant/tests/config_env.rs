//! `QdrantMemoryConfig` env-source defaults + overrides.
//!
//! These tests exercise the pure `from_source` core that backs `from_env`, so the
//! suite never mutates process-global environment variables.

use ardur_memory_qdrant::QdrantMemoryConfig;

fn from_pairs(pairs: &[(&str, &str)]) -> QdrantMemoryConfig {
    QdrantMemoryConfig::from_source(|key| {
        pairs
            .iter()
            .find_map(|(candidate, value)| (*candidate == key).then(|| (*value).to_string()))
    })
}

#[test]
fn from_source_defaults_when_env_source_is_empty() {
    assert_eq!(
        QdrantMemoryConfig::from_source(|_| None),
        QdrantMemoryConfig::default()
    );
}

#[test]
fn from_source_reads_overrides_without_touching_process_env() {
    let cfg = from_pairs(&[
        ("QDRANT_URL", "http://q:6334"),
        ("QDRANT_API_KEY", "k"),
        ("QDRANT_COLLECTION", "c"),
        ("QDRANT_VECTOR_DIM", "768"),
        ("EMBED_MODEL", "gte-base-en-v1.5"),
    ]);

    assert_eq!(cfg.url, "http://q:6334");
    assert_eq!(cfg.api_key.as_deref(), Some("k"));
    assert_eq!(cfg.collection_name, "c");
    assert_eq!(cfg.vector_dim, 768);
    assert_eq!(cfg.default_embed_model.as_deref(), Some("gte-base-en-v1.5"));
}

#[test]
fn from_source_treats_empty_values_as_unset_and_ignores_malformed_dim() {
    let cfg = from_pairs(&[
        ("QDRANT_VECTOR_DIM", "not-a-number"),
        ("QDRANT_API_KEY", ""),
    ]);

    assert_eq!(
        cfg.vector_dim, 384,
        "a malformed dim falls back to the default"
    );
    assert_eq!(cfg.api_key, None, "an empty api key reads as absent");
}
