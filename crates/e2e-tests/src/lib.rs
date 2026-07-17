//! ardur-e2e-tests — the workspace's cross-crate end-to-end scenario host.
//!
//! This crate carries no public API. It exists only to host the integration
//! targets under `tests/scenario_NN_<name>.rs` and the shared [`fixtures`] they
//! build on. See `README.md` for the scenario catalog and
//! `architect/backlog/e2e-test-coverage-gaps.md` for the coverage rationale.
#![forbid(unsafe_code)]

pub mod fixtures;
