//! Source guards for the router's fail-closed invariants (#411 / #531 D0).
//!
//! These read `src/router.rs` at RUNTIME (never `include_str!`, which caches
//! at compile time and would keep testing the previously compiled source).
//! The compiler alone already enforces exhaustiveness *today*; this guard is
//! what fails the build when someone adds a `_ =>` arm while adding a variant
//! arm to silence the exhaustiveness error — the exact mutation that turns
//! "new variant forces a decision" into "new variant silently surfaces".

use std::path::PathBuf;

fn router_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("router.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Slice the body of `fn classify` out of the source: from its signature to
/// the start of the next item. Keeping the window tight means a wildcard arm
/// added to some *other* match does not trip this guard — and deleting
/// `classify` outright is caught by the missing-marker panic.
fn classify_body(src: &str) -> &str {
    let start = src
        .find("fn classify(")
        .expect("fn classify must exist in router.rs");
    let rest = &src[start..];
    let end = rest
        .find("\nfn ")
        .expect("classify is not the last item in router.rs");
    &rest[..end]
}

#[test]
fn guard_classify_has_no_wildcard_arm() {
    let src = router_source();
    let body = classify_body(&src);
    assert!(
        !body.contains("_ =>"),
        "classify grew a wildcard arm — a future ProviderError variant would \
         be silently classified instead of breaking the build:\n{body}"
    );
}

#[test]
fn guard_classify_covers_every_provider_error_variant() {
    let src = router_source();
    let body = classify_body(&src);
    // The nine variants of ProviderError, each required to appear in
    // classify's match. Deleting an arm (and adding `_ =>` to compensate)
    // fails BOTH this guard and the no-wildcard guard.
    let variants = [
        "ProviderError::RateLimited",
        "ProviderError::NetworkFailure",
        "ProviderError::InvalidRequest",
        "ProviderError::ModelNotAvailable",
        "ProviderError::CostCeilingExceeded",
        "ProviderError::Unauthorized",
        "ProviderError::Upstream",
        "ProviderError::InvalidSelection",
        "ProviderError::UnknownProvider",
    ];
    // Population pin: the guard examines the full variant set.
    assert_eq!(variants.len(), 9, "ProviderError has 9 variants");
    for variant in variants {
        assert!(
            body.contains(variant),
            "classify no longer names {variant} — its failover decision is \
             no longer explicit:\n{body}"
        );
    }
}

#[test]
fn guard_ambiguous_upstream_is_not_retryable() {
    let src = router_source();
    // The 5xx sub-classifier must exist and be consulted inside classify: it
    // is the only thing standing between "HTTP 503" (advance) and an
    // arbitrary upstream message (surface).
    assert!(
        src.contains("fn funneled_http_status"),
        "funneled_http_status was removed — Upstream classification is unbounded"
    );
    let body = classify_body(&src);
    assert!(
        body.contains("funneled_http_status"),
        "classify no longer consults the funneled-status parser"
    );
}

#[test]
fn guard_receipt_honesty_markers_exist() {
    let src = router_source();
    // The failed-path marker receipts carry when the chain exhausts. Its
    // removal would make failed turns indistinguishable from served ones.
    assert!(
        src.contains("router:failed("),
        "the failed-path receipt marker is gone from name()"
    );
}
