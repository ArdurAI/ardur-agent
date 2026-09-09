//! Incremental verification cache for an append-only receipt log.
//!
//! [`verify_persisted_chain_with_jwks`] is O(chain): every compact JWS is
//! ES256-checked. The server used to pay that cost on every `/chat` turn (twice)
//! and every `/metrics` scrape, so a long-lived process degraded linearly with
//! the log (#355).
//!
//! Every load re-reads the log (IO is cheap next to ES256). The cache then
//! compares the loaded compact-JWS bytes and the caller's JWKS against the last
//! verified snapshot:
//!
//! * identical JWS + same JWKS → hit, skip signatures
//! * trusted prefix + growth → authenticate only the tail
//! * rewrite, shrink, JWKS change, or prior verify failure → full re-verify
//!
//! Caching is keyed on the loaded contents, not `(len, mtime)`, so a same-size
//! rewrite or a torn read vs a concurrent append cannot poison the next scrape.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
    jwks: Jwks,
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
    /// Times a load skipped ES256 because the on-disk compact-JWS bytes matched a
    /// previously verified chain under the same JWKS.
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
                jwks: Jwks::new(),
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

    /// Load `path` and authenticate it against `jwks`, reusing prior ES256 work
    /// when the loaded compact-JWS bytes have not changed (or have only grown)
    /// under the same JWKS.
    ///
    /// Every call re-reads the log so a concurrent append or same-size rewrite
    /// cannot be cached under stale metadata. A missing file is an empty,
    /// verified chain. I/O and malformed-line failures surface as
    /// [`ReceiptChainError`]; a signature/linkage failure is returned as
    /// [`LoadedReceiptChain::verified`] `== false` rather than `Err`, matching
    /// the `/metrics` scrape contract.
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
        let chain = load_persisted_chain(path)?;

        if let Some(hit) = self.try_content_hit(path, jwks, &chain) {
            return Ok(hit);
        }

        let prefix_len = {
            let inner = self.inner.lock();
            let same_path = inner.path.as_deref() == Some(path);
            let same_jwks = inner.jwks == *jwks;
            let prefix_matches = same_path
                && same_jwks
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

        {
            let mut inner = self.inner.lock();
            inner.path = Some(path.to_path_buf());
            inner.jwks = jwks.clone();
            inner.receipts = receipts.clone();
            inner.verify_error = verify_error.clone();
        }

        Ok(LoadedReceiptChain {
            receipts,
            verify_error,
        })
    }

    fn try_content_hit(
        &self,
        path: &Path,
        jwks: &Jwks,
        chain: &[PersistedReceipt],
    ) -> Option<LoadedReceiptChain> {
        let inner = self.inner.lock();
        let exact = inner.path.as_deref() == Some(path)
            && inner.jwks == *jwks
            && inner.verify_error.is_none()
            && inner.receipts.len() == chain.len()
            && inner
                .receipts
                .iter()
                .zip(chain.iter())
                .all(|(cached, loaded)| cached.jws_compact == loaded.jws_compact);
        if !exact {
            return None;
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(LoadedReceiptChain {
            receipts: inner.receipts.clone(),
            verify_error: None,
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

        // A different valid chain of the same length is a rewrite, not an append.
        // Content comparison (not len/mtime) is what detects this.
        write_log(&path, &sign_chain(&key, 2));
        let loaded = cache.load(&path, &jwks).expect("rewritten load");
        assert!(loaded.verified());
        assert_eq!(
            cache.stats(),
            ReceiptCacheStats {
                hits: 0,
                receipts_fully_verified: 4,
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

    #[test]
    fn jwks_change_forces_a_full_reverify() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("chain.jsonl");
        let key = Es256SigningKey::generate();
        let jwks = Jwks::from_public_key(&key.public_key());
        write_log(&path, &sign_chain(&key, 2));

        let cache = VerifiedReceiptCache::new();
        assert!(cache.load(&path, &jwks).expect("trusted load").verified());

        let other = Jwks::from_public_key(&Es256SigningKey::generate().public_key());
        let loaded = cache.load(&path, &other).expect("rotated load");
        assert!(
            !loaded.verified(),
            "a chain verified under key A must not report verified under key B"
        );
        assert_eq!(
            cache.stats(),
            ReceiptCacheStats {
                hits: 0,
                receipts_fully_verified: 4,
                receipts_tail_verified: 0,
            }
        );
    }
}
