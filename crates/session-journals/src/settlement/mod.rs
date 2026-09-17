//! Typed synchronous settlement evidence, not a persistent budget ledger.
//!
//! Each UUID-named file is the latest **cumulative** turn snapshot. Count a
//! round once, not once per revision/projection/receipt. Known incurred cost
//! is independent of requested/applied caller debit; operator-borne known
//! expense is the per-axis positive part of known incurred minus net applied
//! debit after actual rollback. Headroom-clamped credits can leave applied
//! capacity debit ABOVE requested debit or known cost; retain that excess as
//! actual movement, not negative operator expense or fabricated collection.
//! Release/rollback operations that remain pending belong in `Unresolved`
//! (`ReleaseCreditPending` / `RollbackCreditPending`), not `Settled`, even when
//! a budget API returned `Ok`. These types do not classify such API results.
//! The current contract is full reserve release/full applied-debit rollback,
//! not an arbitrary partial final refund. A rollback's remaining credit is the
//! applied-debit tuple minus its actual credited tuple, on all five axes.
//! Before `Settled`, actual reserved credit must cover the positive part of
//! reserve minus requested debit on every axis (the entire reserve for zero
//! debit), unless a complete applied-debit rollback compensates it. A genuine
//! debit shortfall on a different axis does not cancel an unpaid release.
//! Headroom-clamped partial credits remain valid unresolved observations and
//! may progress monotonically; no nominal credit is invented to close them.
//!
//! Provider observations form one cumulative series: each cost axis and each
//! reported usage component may only increase; existing usage and provenance
//! cannot disappear or be replaced. Dispatch intent cannot become no-dispatch.
//! Finished/interrupted flags are sticky historical facts. A late completion
//! may add `finished` while retaining `interrupted`; it does not erase the
//! interruption or assert that every unreported tail is now known. Changing a
//! pricing source requires richer evidence than this schema, not replacement.
//! An interrupted tool may refine to an actual returned `Completed` result
//! (including its digest and actual cost), or `Failed` retaining effect-unknown.
//! It cannot become pre-invoke refusal, intent, or a no-effect failure. The
//! runtime must verify those late results; the store does not authenticate them.
//! A mere tool dispatch intent may resolve to a verified pre-invoke refusal.
//! These evidence refinements do not select a new caller debit/refund policy.
//!
//! The writer owns a stable `writer.lock` lease for its lifetime. Every writer
//! sharing the receipt root must cooperate; this crate does not coordinate
//! independent receipt appenders. Readers take no exclusive lease and their
//! inventory is not a cross-file transaction. Corrupt/unreadable inventory
//! returns an error, never an empty successful result. Missing roots are errors.
//!
//! A lost acknowledgement is resolved only by exact-byte comparison AND
//! successful file and parent sync. Failed verification/sync quarantines the
//! live writer. Only resolving/retrying that same pending attempt may clear
//! its I/O quarantine; unrelated writes cannot. A `Durable` result attests to
//! storage, not authentic receipt append or successful economic application.
//! Every public `Unresolved` result closes admission, including an unknown or
//! mismatched exact-resolution request on an otherwise healthy writer. Without
//! a retained pending attempt there are no same-attempt retry bytes to resolve.
//! `StoreHealth` is storage admission health, not economic closure.
//! Receipt candidates are prepared evidence only. The receipt layer must verify
//! signatures, parents, offsets and actual append before supplying bindings.
//!
//! Opened final-root descriptors must be owned by the effective runtime UID
//! with exactly mode 0700; retained regular files (lease, format, snapshots)
//! must have the same owner and exactly mode 0600. Special mode bits are refused.
//! Existing unsuitable state is rejected, never chmod/chown-repaired. Checks
//! recur on inventory and live integrity paths. Ancestors are traversed without
//! following symlinks, but are NOT required to be 0700: callers must supply
//! trusted, durably established ancestors and a cooperative stable namespace.
//! These mode/UID checks do not inspect ACLs or establish ACL confidentiality;
//! callers must exclude ACL grants to other principals. This is not protection
//! against arbitrary hostile same-UID namespace mutation or every path race.
//!
//! Supported durability is the local Unix file/sync/rename model on Linux and
//! macOS. Other platforms explicitly refuse opening/reading this store. The
//! filesystem must honor these operations; network filesystems are not given
//! stronger guarantees. Synchronous I/O can block indefinitely. There is no
//! arbitrary-crash losslessness, persistent token budget, effect replay, or
//! cross-file/budget transaction guarantee.
//!
//! All retained files are bounded and validated, and acknowledged identities
//! are remembered during the writer lifetime. No durable membership index is
//! added: an undetectably deleted/rolled-back whole root or previously unseen
//! historical file cannot be detected by directory scanning. Inventory reports
//! retained-file coverage only. History is never automatically pruned; limits
//! refuse further work rather than delete evidence. The default count/record-byte
//! product is a formal bound, NOT a practical memory budget: inventory retains
//! decoded records and encodings, and integrity checks repeat that scan. Runtime
//! capacity planning must choose smaller limits; an aggregate byte budget or
//! streaming validation is a separate bounded follow-up, not a new database,
//! index or pruning policy in this layer. Keep raw arguments, output,
//! bearer tokens and diagnostic dumps out of metadata fields.
//!
//! Not every observation is encodable: checked aggregate overflow and post-work
//! bounds reject even an `Unresolved` snapshot. Rejected new observations are
//! not thereby durable. The runtime owner must reserve evidence capacity before
//! dispatch, retain raw observations and fail closed on rejection; zero or
//! saturated totals are not substitutes for an unavailable exact total.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod store;
mod types;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use store::*;
pub use types::*;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod unsupported {
    use super::*;
    use ardur_core_types::Sha256Digest;
    use std::{convert::Infallible, path::Path};

    /// Unconstructible writer on unsupported durability platforms.
    pub struct FileSettlementStore(Infallible);
    impl FileSettlementStore {
        /// Refuse rather than claim equivalent durable semantics.
        pub fn open(
            _: &Path,
            _: ReceiptIdentity,
            _: SettlementLimits,
        ) -> Result<Self, SettlementStoreError> {
            Err(SettlementStoreError::UnsupportedPlatform)
        }
        /// No writer can be constructed on this platform.
        pub fn health(&self) -> StoreHealth {
            match self.0 {}
        }
        /// No writer can be constructed on this platform.
        pub fn put_exact(&mut self, _: Option<u64>, _: &EncodedSnapshot) -> WriteResolution {
            match self.0 {}
        }
        /// No writer can be constructed on this platform.
        pub fn resolve_exact(&mut self, _: TurnId, _: u64, _: Sha256Digest) -> WriteResolution {
            match self.0 {}
        }
    }
    /// Refuse unsupported platform durability, never return empty coverage.
    pub fn load_settlement_snapshot(
        _: &Path,
        _: &ReceiptIdentity,
        _: &SettlementLimits,
    ) -> Result<SettlementInventory, SettlementStoreError> {
        Err(SettlementStoreError::UnsupportedPlatform)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn unsupported_platform_refuses_writer_and_inventory() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("store");
            let identity = ReceiptIdentity {
                receipt_log: temp.path().join("chain.jsonl"),
                signer: Sha256Digest::of(b"public signer"),
            };
            let limits = SettlementLimits::default();
            assert!(matches!(
                FileSettlementStore::open(&root, identity.clone(), limits),
                Err(SettlementStoreError::UnsupportedPlatform)
            ));
            assert!(matches!(
                load_settlement_snapshot(&root, &identity, &limits),
                Err(SettlementStoreError::UnsupportedPlatform)
            ));
            assert!(!root.exists());
        }
    }
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub use unsupported::*;
