//! ardur-core-types — the workspace's shared value primitives.
//!
//! Plan family: §0.0 (`plans/0.0-workspace-scaffold-blueprint.md`), R-2
//! reconciliation. This crate is the single owner of the ids, unit-bearing
//! newtypes, and the D-4 cost tuple that were previously re-defined — and
//! quietly diverged — across `ardur-runtime`, `ardur-receipt`,
//! `ardur-cost-gate`, `ardur-cap-token`, and `ardur-memory`. Each of those
//! crates now re-exports the canonical type from here instead of minting its
//! own, so a value that flows runtime → cost-gate → receipt is *one* type end
//! to end and cannot be silently truncated at a crate boundary.
//!
//! It is deliberately the leaf-most crate in the workspace: its only
//! dependencies are `serde`, `sha2`, and `uuid`, so every other crate can
//! depend on it without introducing a cycle.
//!
//! # What lives here
//!
//! - [`HolderId`], [`ProviderId`], [`ModelId`], [`ReceiptId`], [`TokenId`],
//!   [`UnixTsMillis`] — the shared id and unit newtypes.
//! - [`Sha256Digest`] — a 32-byte digest with **one** canonical wire form
//!   (lowercase hex), so the digests that feed signed, hash-chained receipts
//!   serialize identically everywhere.
//! - [`CostTuple`], [`CostEnvelope`], [`CostDelta`] — the "cost as protocol
//!   primitive" trio, with `attention_score` represented as a fixed-point
//!   integer (see [`CostTuple::attention_score`]) so cost keeps `Eq`, exact
//!   ledger arithmetic, and byte-stable receipts at once.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod cost;
mod digest;
mod ids;

pub use cost::{CostDelta, CostEnvelope, CostTuple, MILLI_ATTENTION_PER_UNIT};
pub use digest::{DigestParseError, Sha256Digest};
pub use ids::{HolderId, ModelId, ProviderId, ReceiptId, TokenId, UnixTsMillis};
