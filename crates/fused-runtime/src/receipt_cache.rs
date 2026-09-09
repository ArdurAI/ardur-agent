//! Incremental verification cache for an append-only receipt log.
//!
//! [`verify_persisted_chain_with_jwks`] is O(chain): every compact JWS is
//! ES256-checked. The server used to pay that cost on every `/chat` turn (twice)
//! and every `/metrics` scrape, so a long-lived process degraded linearly with
//! the log (#355).
//!
//! This cache keys on `(path, file length, mtime)`. An unchanged file is a hit
//! and skips both the re-read and the signatures. When the file has only grown
//! and the previously verified prefix is byte-identical, only the appended tail
//! is authenticated. A shrink, rewrite, or prefix mismatch forces a full
//! re-verify.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use ardur_receipt::Jwks;
use parking_lot::Mutex;

use crate::receipts::{
    PersistedReceipt, ReceiptChainError, load_persisted_chain, verify_persisted_chain_range,
};

/// Cached, incrementally-verified view of one on-disk receipt chain.
#[derive(Debug)]
pub struct VerifiedReceiptCache {
    inner: Mutex<CacheInner>,
    hits: AtomicU64,
    receipts_fully_verified: AtomicU64,
    receipts_tail_verified: AtomicU64,
}

#[derive(Debug)]
struct CacheInner {
    path: Option<PathBuf>,
    file_len: u64,
    modified: Option<SystemTime>,
    receipts: Arc<Vec<PersistedReceipt>>,
    verify_error: Option<String>,
}

/// A chain loaded (and, when possible, incrementally verified) through
/// [`VerifiedReceiptCache::load`].
#[derive(Clone, Debug)]
pub struct LoadedReceiptChain {
    receipts: Arc<Vec<PersistedReceipt>>,
    verify_error: Option<String>,
}

impl LoadedReceiptChain {
    /// The receipts in append order. Present even when verification failed, so
    /// `/metrics` can still roll up aggregates and flag `chain_verified = false`.
    #[must_use]
    pub fn receipts(&self) -> &[PersistedReceipt] {
        &self.receipts
    }

    /// Number of receipts in the loaded chain.
    #[must_use]
    pub fn len(&self) -> usize {
        self.receipts.len()
    }

    /// Whether the chain is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty()
    }

    /// `true` when every receipt authenticated (ES256 + parent-hash linkage).
    #[must_use]
    pub fn verified(&self) -> bool {
        self.verify_error.is_none()
    }

    /// The verification failure, if any, formatted for logs.
    #[must_use]
    pub fn verify_error(&self) -> Option<&str> {
        self.verify_error.as_deref()
    }
}

/// Instrumentation counters for the #355 regression tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiptCacheStats {
    /// Times a load returned the cached chain without touching disk or JWKS.
    pub hits: u64,
    /// Receipts authenticated by a full-chain verify.
    pub receipts_fully_verified: u64,
    /// Receipts authenticated as an appended tail on top of a trusted prefix.
    pub receipts_tail_verified: u64,
}

impl Default for VerifiedReceiptCache {
    fn default() -> Self {
        Self::new()
    }
}

impl VerifiedReceiptCache {
    /// An empty cache. The first [`load`](Self::load) populates it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                path: None,
                file_len: 0,
                modified: None,
                receipts: Arc::new(Vec::new()),
                verify_error: None,
            }),
            hits: AtomicU64::new(0),
            receipts_fully_verified: AtomicU64::new(0),
            receipts_tail_verified: AtomicU64::new(0),
        }
    }

    /// Snapshot of hit/verify counters. Tests use this to prove a second load of
    /// an unchanged file does not re-run ES256; production callers can ignore it.
    #[must_use]
    pub fn stats(&self) -> ReceiptCacheStats {
        ReceiptCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            receipts_fully_verified: self.receipts_fully_verified.load(Ordering::Relaxed),
            receipts_tail_verified: self.receipts_tail_verified.load(Ordering::Relaxed),
        }
    }

    /// Load `path` and authenticate it against `jwks`, reusing prior work when
    /// the file has not changed (or has only grown).
    ///
    /// A missing file is an empty, verified chain. I/O and malformed-line
    /// failures surface as [`ReceiptChainError`]; a signature/linkage failure is
    /// returned as [`LoadedReceiptChain::verified`] `== false` rather than `Err`,
    /// matching the `/metrics` scrape contract.
    ///
    /// # Errors
    /// [`ReceiptChainError::Io`] or [`ReceiptChainError::Malformed`] from the
    /// underlying loader.
    pub fn load(
        &self,
        path: impl AsRef<Path>,
        jwks: &Jwks,
    ) -> Result<LoadedReceiptChain, ReceiptChainError> {
        let path = path.as_ref();
        let meta = match std::fs::metadata(path) {
            Ok(meta) => Some(meta),
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => return Err(ReceiptChainError::Io(err)),
        };

        if let Some(hit) = self.try_hit(path, meta.as_ref()) {
            return Ok(hit);
        }

        let chain = load_persisted_chain(path)?;
        // Torn-tail repair inside the loader can shrink the file; re-stat so the
        // cache key matches what is actually on disk now.
        let meta = match std::fs::metadata(path) {
            Ok(meta) => Some(meta),
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => return Err(ReceiptChainError::Io(err)),
        };

        let prefix_len = {
            let inner = self.inner.lock();
            let same_path = inner.path.as_deref() == Some(path);
            let prefix_matches = same_path
                && inner.verify_error.is_none()
                && inner.receipts.len() <= chain.len()
                && inner
                    .receipts
                    .iter()
                    .zip(chain.iter())
                    .all(|(cached, loaded)| cached.jws_compact == loaded.jws_compact);
            if prefix_matches {
                inner.receipts.len()
            } else {
                0
            }
        };

        let verify = verify_persisted_chain_range(&chain, jwks, prefix_len);
        if prefix_len == 0 {
            self.receipts_fully_verified
                .fetch_add(chain.len() as u64, Ordering::Relaxed);
        } else {
            self.receipts_tail_verified
                .fetch_add((chain.len() - prefix_len) as u64, Ordering::Relaxed);
        }

        let verify_error = verify.err().map(|err| err.to_string());
        let receipts = Arc::new(chain);
        let (file_len, modified) = match meta {
            Some(meta) => (meta.len(), meta.modified().ok()),
            None => (0, None),
        };

        {
            let mut inner = self.inner.lock();
            inner.path = Some(path.to_path_buf());
            inner.file_len = file_len;
            inner.modified = modified;
            inner.receipts = receipts.clone();
            inner.verify_error = verify_error.clone();
        }

        Ok(LoadedReceiptChain {
            receipts,
            verify_error,
        })
    }

    fn try_hit(&self, path: &Path, meta: Option<&std::fs::Metadata>) -> Option<LoadedReceiptChain> {
        let inner = self.inner.lock();
        if inner.path.as_deref() != Some(path) {
            return None;
        }
        let is_hit = match meta {
            Some(meta) => inner.file_len == meta.len() && inner.modified == meta.modified().ok(),
            None => {
                inner.file_len == 0 && inner.receipts.is_empty() && inner.verify_error.is_none()
            }
        };
        if !is_hit {
            return None;
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(LoadedReceiptChain {
            receipts: inner.receipts.clone(),
            verify_error: inner.verify_error.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use ardur_receipt::{
        CostTuple, Es256SigningKey, HolderId, ReceiptBody, ReceiptSigner, Sha256Digest, TokenId,
        UnixTsMillis, VerbObject,
    };

    use super::*;

    fn sample_body(parent_hash: Option<Sha256Digest>) -> ReceiptBody {
        ReceiptBody {
            receipt_id: uuid::Uuid::new_v4(),
            parent_hash,
            verb: VerbObject::new("cost.admission.allow.v1").expect("valid verb"),
            issued_at: UnixTsMillis(1_700_000_000_000),
            subject: HolderId("spiffe://ardur/test".to_string()),
            cap_token_id: TokenId(uuid::Uuid::from_u128(0x355)),
            payload_digest: Sha256Digest::of(b"payload"),
            session_id: None,
            cost: CostTuple {
                tokens_in: 0,
                tokens_out: 0,
                cents: 0,
                wall_ms: 0,
                attention_score: 0,
            },
            tool_calls: Vec::new(),
            provider: Some("test-provider".to_string()),
        }
    }

    fn sign_chain(key: &Es256SigningKey, n: usize) -> Vec<String> {
        let mut lines = Vec::with_capacity(n);
        let mut prev: Option<String> = None;
        for _ in 0..n {
            let parent = prev.as_ref().map(|jws| Sha256Digest::of(jws.as_bytes()));
            let signed = ReceiptSigner::sign(sample_body(parent), key).expect("sign");
            let jws = signed.jws_compact().to_string();
            prev = Some(jws.clone());
            lines.push(jws);
        }
        lines
    }

    fn write_log(path: &Path, lines: &[String]) {
        let mut body = lines.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        std::fs::write(path, body).expect("write chain");
    }

    #[test]
    fn unchanged_file_is_a_hit_and_does_not_reverify() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chain.jsonl");
        let key = Es256SigningKey::generate();
        let jwks = Jwks::from_public_key(&key.public_key());
        write_log(&path, &sign_chain(&key, 3));

        let cache = VerifiedReceiptCache::new();
        let first = cache.load(&path, &jwks).expect("first load");
        assert!(first.verified());
        assert_eq!(first.len(), 3);
        assert_eq!(
            cache.stats(),
            ReceiptCacheStats {
                hits: 0,
                receipts_fully_verified: 3,
                receipts_tail_verified: 0,
            }
        );

        let second = cache.load(&path, &jwks).expect("second load");
        assert!(second.verified());
        assert_eq!(second.len(), 3);
        assert_eq!(
            cache.stats(),
            ReceiptCacheStats {
                hits: 1,
                receipts_fully_verified: 3,
                receipts_tail_verified: 0,
            },
            "a second load of an unchanged chain must not re-run ES256"
        );
    }

    #[test]
    fn append_verifies_only_the_tail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chain.jsonl");
        let key = Es256SigningKey::generate();
        let jwks = Jwks::from_public_key(&key.public_key());
        let mut lines = sign_chain(&key, 2);
        write_log(&path, &lines);

        let cache = VerifiedReceiptCache::new();
        cache.load(&path, &jwks).expect("prefix load");
        assert_eq!(cache.stats().receipts_fully_verified, 2);

        let parent = Sha256Digest::of(lines.last().expect("prefix").as_bytes());
        let signed = ReceiptSigner::sign(sample_body(Some(parent)), &key).expect("sign tail");
        lines.push(signed.jws_compact().to_string());
        write_log(&path, &lines);

        let loaded = cache.load(&path, &jwks).expect("tail load");
        assert!(loaded.verified());
        assert_eq!(loaded.len(), 3);
        assert_eq!(
            cache.stats(),
            ReceiptCacheStats {
                hits: 0,
                receipts_fully_verified: 2,
                receipts_tail_verified: 1,
            },
            "growth of an append-only log must authenticate only the new receipt"
        );
    }

    #[test]
    fn rewritten_prefix_forces_a_full_reverify() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chain.jsonl");
        let key = Es256SigningKey::generate();
        let jwks = Jwks::from_public_key(&key.public_key());
        write_log(&path, &sign_chain(&key, 2));

        let cache = VerifiedReceiptCache::new();
        cache.load(&path, &jwks).expect("first load");

        // A longer chain whose prefix JWS does not match the cached prefix is a
        // rewrite, not an append — length change also defeats the mtime cache.
        write_log(&path, &sign_chain(&key, 3));
        let loaded = cache.load(&path, &jwks).expect("rewritten load");
        assert!(loaded.verified());
        assert_eq!(
            cache.stats(),
            ReceiptCacheStats {
                hits: 0,
                receipts_fully_verified: 5,
                receipts_tail_verified: 0,
            },
            "a rewritten prefix must not be trusted as an incremental tail"
        );
    }

    #[test]
    fn tampered_tail_is_not_verified_and_is_not_a_hit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chain.jsonl");
        let key = Es256SigningKey::generate();
        let jwks = Jwks::from_public_key(&key.public_key());
        let mut lines = sign_chain(&key, 1);
        write_log(&path, &lines);

        let cache = VerifiedReceiptCache::new();
        assert!(cache.load(&path, &jwks).expect("genesis").verified());

        let other = Es256SigningKey::generate();
        let forged = ReceiptSigner::sign(
            sample_body(Some(Sha256Digest::of(lines[0].as_bytes()))),
            &other,
        )
        .expect("sign with the wrong key");
        lines.push(forged.jws_compact().to_string());
        write_log(&path, &lines);

        let loaded = cache.load(&path, &jwks).expect("tampered load");
        assert!(!loaded.verified(), "a forged tail must fail verification");
        assert_eq!(loaded.len(), 2, "aggregates still see the loaded lines");
        assert!(loaded.verify_error().is_some());
    }

    #[test]
    fn missing_file_is_an_empty_verified_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.jsonl");
        let key = Es256SigningKey::generate();
        let jwks = Jwks::from_public_key(&key.public_key());
        let cache = VerifiedReceiptCache::new();

        let first = cache.load(&path, &jwks).expect("missing load");
        assert!(first.verified());
        assert!(first.is_empty());

        let second = cache.load(&path, &jwks).expect("missing hit");
        assert!(second.verified());
        assert_eq!(cache.stats().hits, 1);
    }
}
