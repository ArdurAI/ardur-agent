//! ardur-injection-defense — the §11.16 prompt-injection defense layer: a
//! filter that scans inbound user and tool content for injection patterns
//! before the runtime forwards it to a provider.
//!
//! Plan family: §11.16 (`plans/11.16-injection-defense-blueprint.md`),
//! ADR-Phase3-548.
//!
//! # Phase 1 (this crate)
//!
//! Detection is purely rule-based — a table of compiled regex signatures.
//!
//! - [`InjectionFilter`] — the object-safe contract every filter implements:
//!   an async [`InjectionFilter::scan`], its [`FilterId`], and a
//!   [`InjectionFilter::confidence_threshold`].
//! - [`ScannableContent`] — the unit a filter scans (a user message, a tool
//!   output, a fetched web body, or a file body), tagged with its
//!   [`ContentSource`].
//! - [`ScanResult`] / [`Verdict`] — the decision (`Allow`,
//!   `AllowWithSanitization`, or `Block`), the [`InjectionFlag`]s raised, the
//!   max confidence, and the scan duration.
//! - [`PatternBasedFilter`] — the concrete rule engine, seeded with ≥12
//!   built-in signatures spanning every [`FlagCategory`].
//! - [`SanitizingFilter`] — a wrapper that downgrades below-threshold matches
//!   from `Allow` to an `AllowWithSanitization` that `[REDACTED]`s the matched
//!   substrings.
//! - [`FilterRegistry`] — runs a content unit through many filters and
//!   aggregates the verdicts most-restrictive-wins.
//! - [`FilterError`] — the crate's typed-error surface.
//!
//! [`ToolId`](ardur_tool_registry::ToolId) and
//! [`ChannelId`](ardur_messaging_gateway::ChannelId) are re-exported from the
//! tool and messaging layers so a scannable tool output or channel-sourced
//! message shares one schema with the layer that produced it.
//!
//! Rule-based pattern matching is the whole of Phase 1. The inline
//! `// TODO §11.16 Phase 2:` markers below name what comes next.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

// TODO §11.16 Phase 2: ML-based detection — replace (or back-stop) the regex
// table with a trained classifier behind the same `InjectionFilter` surface.
// TODO §11.16 Phase 2: contextual rewriting — sanitize by neutralizing the
// injection in place rather than blunt `[REDACTED]` substitution.
// TODO §11.16 Phase 2: per-source policies — vary strictness by
// `ContentSource` (trust `Direct` REPL input more than a `Webhook` payload or
// a nested `ToolReturn`).
// DONE ARD-48: a `FilterRegistry` is wired into `ardur-fused-runtime`'s
// `FusedRuntime::submit` as stage 4.5 — every outbound prompt is scanned after
// the pre-submit hooks and before the provider dispatch. Tool-output scanning
// (the `ToolReturn` path) follows once tool-use lands (TODO ARD-22).

mod content;
mod error;
mod filter;
mod flag;
mod pattern;
mod registry;
mod result;
mod sanitize;

pub use content::{ContentSource, ScannableContent};
pub use error::FilterError;
pub use filter::{FilterId, InjectionFilter};
pub use flag::{FlagCategory, InjectionFlag};
pub use pattern::{CompiledPattern, DEFAULT_THRESHOLD, PatternBasedFilter};
pub use registry::FilterRegistry;
pub use result::{CombinedScanResult, ScanResult, Verdict};
pub use sanitize::{REDACTION, SanitizingFilter};

// Shared newtypes owned by the tool (§6.0) and messaging (§4.0) layers;
// re-exported so callers wiring scannable content reference one schema.
pub use ardur_messaging_gateway::ChannelId;
pub use ardur_tool_registry::ToolId;
