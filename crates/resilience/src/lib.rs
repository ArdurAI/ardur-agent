//! Shared resilience primitives for ardur-agent's external call boundaries:
//! model providers, messaging channels, MCP/tool calls, and outbound
//! webhooks.
//!
//! Three independent, composable pieces:
//!
//! - [`retry`] — retry with exponential backoff + full jitter
//!   ([`RetryPolicy`], [`retry_with_backoff`]).
//! - [`timeout`] — bound a future's wall-clock time ([`with_timeout`]).
//! - [`circuit_breaker`] — a self-expiring circuit breaker
//!   ([`CircuitBreaker`]) that fails fast without invoking the wrapped
//!   operation while open.
//!
//! # The fail-closed contract
//!
//! None of these primitives ever turn a failure into a synthesized success.
//! A timeout is an [`Err`], an open breaker is an [`Err`], a retry exhausting
//! its attempts returns the last [`Err`]. This matters most at
//! security-relevant call sites (capability/policy checks — see
//! `ardur-cap-token`'s `FileDenyList` and `ardur-cedar-policy`'s
//! `CedarPolicyBundle::evaluate`, both of which already map internal errors
//! to a deny/indeterminate outcome rather than an allow): wrapping such a
//! check in these combinators preserves that contract for free, because the
//! combinators propagate `Err` rather than absorb it. A caller that already
//! treats "check failed" as "deny" keeps doing so when the failure now also
//! covers "timed out" or "breaker open" — there is no new code path that
//! could accidentally resolve a fault to "allow". See
//! `tests/fail_closed.rs` for a worked example.
//!
//! Retrying is the one primitive that needs an explicit per-call decision
//! ([`retry_with_backoff`]'s `is_retryable` predicate): a caller on a
//! security path should mark policy denials as **not** retryable, so a
//! transient-fault retry loop never repeatedly probes a deny into a
//! different answer. [`RetryPolicy::none`] is available for call sites that
//! want no retrying at all.

pub mod circuit_breaker;
pub mod retry;
pub mod timeout;

pub use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitError, CircuitState};
pub use retry::{RetryPolicy, retry_with_backoff};
pub use timeout::{Elapsed, with_timeout};
