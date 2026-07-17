//! Canonical tool-argument fingerprinting.
//!
//! The same-tool-same-args signal must treat two calls as identical when they
//! carry the same arguments, regardless of JSON key order or whitespace. The
//! blueprint specifies canonical CBOR (RFC 8949 §4.2); this crate achieves the
//! same stability guarantee — order-independent, whitespace-independent — by
//! recursively key-sorting the JSON value and hashing its compact serialization,
//! keeping the dependency surface to `serde_json` + `sha2`. The property that
//! matters (a stable digest across map-key reordering) holds either way.

use ardur_receipt::Sha256Digest;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Fingerprint a tool call's arguments into a stable [`Sha256Digest`].
///
/// Object keys are sorted recursively before hashing, so `{"a":1,"b":2}` and
/// `{"b":2,"a":1}` — and any whitespace variants — produce the same digest.
/// `exclude` names top-level object fields to drop before hashing (per the
/// per-tool `args_exclude_from_fingerprint` manifest hint): nondeterministic
/// fields like timestamps or request ids that would otherwise defeat repetition
/// detection.
pub fn args_fingerprint(args: &Value, exclude: &[String]) -> Sha256Digest {
    let canonical = canonicalize(args, exclude);
    // `to_vec` on an already-canonicalized value is deterministic: no floats are
    // reordered, keys are emitted in the sorted order we imposed below.
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    Sha256Digest(Sha256::digest(&bytes).into())
}

fn canonicalize(value: &Value, exclude: &[String]) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map
                .iter()
                .filter(|(k, _)| !exclude.iter().any(|e| e == *k))
                .collect();
            entries.sort_by_key(|(k, _)| *k);
            let mut sorted = serde_json::Map::with_capacity(entries.len());
            for (k, v) in entries {
                // `exclude` only applies at the top level, matching the manifest
                // hint's field-name semantics.
                sorted.insert(k.clone(), canonicalize(v, &[]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| canonicalize(v, &[])).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_order_does_not_change_fingerprint() {
        let a = json!({"query": "rust", "limit": 10, "nested": {"x": 1, "y": 2}});
        let b = json!({"nested": {"y": 2, "x": 1}, "limit": 10, "query": "rust"});
        assert_eq!(args_fingerprint(&a, &[]), args_fingerprint(&b, &[]));
    }

    #[test]
    fn distinct_args_differ() {
        let a = json!({"query": "rust"});
        let b = json!({"query": "go"});
        assert_ne!(args_fingerprint(&a, &[]), args_fingerprint(&b, &[]));
    }

    #[test]
    fn excluded_top_level_field_is_ignored() {
        let a = json!({"query": "rust", "request_id": "abc"});
        let b = json!({"query": "rust", "request_id": "zzz"});
        let exclude = vec!["request_id".to_string()];
        assert_eq!(
            args_fingerprint(&a, &exclude),
            args_fingerprint(&b, &exclude)
        );
        // Without the exclusion the differing field must matter.
        assert_ne!(args_fingerprint(&a, &[]), args_fingerprint(&b, &[]));
    }
}
