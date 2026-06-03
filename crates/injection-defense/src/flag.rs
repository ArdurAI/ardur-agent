//! The per-match injection signal — what was matched, by which pattern, and
//! how strongly it indicates an injection attempt.
//!
//! These types are **owned by `ardur-runtime`** (ARD-48) and re-exported here.
//! [`RuntimeError::InjectionBlocked`](ardur_runtime::RuntimeError::InjectionBlocked)
//! carries the flags that justified a block, so the error surface that names
//! them must own them — `ardur-runtime` cannot depend on this crate (that is a
//! cycle: injection-defense already depends, transitively via
//! `ardur-tool-registry` / `ardur-messaging-gateway`, on `ardur-runtime`). So
//! the flag types live there and this crate re-exports them, keeping the public
//! API of injection-defense unchanged.

pub use ardur_runtime::{FlagCategory, InjectionFlag};
