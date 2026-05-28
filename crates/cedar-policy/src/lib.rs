//! ardur-cedar-policy — the Cedar policy-bundle substrate (a thin wrapper
//! around the external `cedar-policy` engine).
//!
//! Plan family: §11.0 (`plans/11.0-gateway-policy-foundation-blueprint.md`).
//!
//! PHASE 0: contracts only. No implementation bodies — every trait method is
//! `unimplemented!()`. The public trait surface is FROZEN against §11.0;
//! widening it is a §0.0 amendment. The build-time-locked policy bundle and
//! the admission surface land in §11.0 Phase 1.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::Path;

use anyhow::Result;

/// Re-exported Cedar engine primitives. The bundle wraps `Authorizer` +
/// `PolicySet` + `Entities` and answers `CedarRequest`s with a `Decision`.
pub use cedar_policy::{Authorizer, Decision, Entities, PolicySet, Request as CedarRequest};

/// A loaded, build-time-locked Cedar policy bundle. Implementors hold the
/// compiled `PolicySet` + `Entities` and answer authorization requests.
pub trait PolicyBundle {
    /// Load a policy bundle from a directory on disk.
    fn load(path: &Path) -> Result<Self>
    where
        Self: Sized,
    {
        let _ = path;
        unimplemented!("Phase 0 contract — body lands in §11.0 Phase 1")
    }

    /// Render an allow/deny decision for `request` against the bundle.
    fn authorize(&self, request: &CedarRequest) -> Decision {
        let _ = request;
        unimplemented!("Phase 0 contract — body lands in §11.0 Phase 1")
    }
}
