//! ardur-benches — the criterion benchmark harness for Ardur's hot paths.
//!
//! This crate carries no runtime code; its purpose is the `benches/` targets
//! (run with `cargo bench -p ardur-benches`). Keeping it a real workspace
//! member means `cargo check --workspace --all-targets` compiles the benchmarks
//! on every CI run, so a signature change in a measured crate breaks the build
//! loudly instead of bit-rotting the harness.
//!
//! See `docs/benchmarks.md` for the recorded baselines and the methodology.
#![forbid(unsafe_code)]
