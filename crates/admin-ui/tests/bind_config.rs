//! ARD-421 — admin-ui bind-address security tests.
//!
//! Verifies the default bind is loopback, non-loopback binds without auth
//! fail, non-loopback binds with auth are allowed, and `--unsafe-bind`
//! overrides the guard.

use std::path::PathBuf;

use ardur_admin::config::{Cli, DEFAULT_BIND, is_loopback, resolve_bind_addr, validate_bind};

/// Build a `Cli` with only the required paths set, simulating defaults for
/// everything else.
fn minimal_cli() -> Cli {
    Cli {
        journal_dir: PathBuf::from("/tmp/journals"),
        receipt_store: PathBuf::from("/tmp/receipts"),
        security_events: None,
        qdrant_url: None,
        qdrant_collection: "ardur_memory".to_string(),
        port: 8090,
        basic_auth: None,
        bind_addr: None,
        unsafe_bind: false,
    }
}

#[test]
fn default_bind_is_loopback() {
    let cli = minimal_cli();
    let addr = resolve_bind_addr(&cli).expect("default resolves");
    assert!(
        is_loopback(&addr),
        "default bind should be loopback, got {addr}"
    );
    assert_eq!(addr.to_string(), DEFAULT_BIND);
}

#[test]
fn non_loopback_bind_without_auth_is_rejected() {
    let mut cli = minimal_cli();
    cli.bind_addr = Some("0.0.0.0".to_string());
    let addr = resolve_bind_addr(&cli).expect("0.0.0.0 parses");
    assert!(!is_loopback(&addr));
    let err = validate_bind(&cli, &addr).expect_err("non-loopback without auth should fail");
    assert!(
        err.contains("--basic-auth") || err.contains("--unsafe-bind"),
        "error should mention --basic-auth or --unsafe-bind: {err}"
    );
}

#[test]
fn non_loopback_bind_with_auth_is_allowed() {
    let mut cli = minimal_cli();
    cli.bind_addr = Some("0.0.0.0".to_string());
    cli.basic_auth = Some("admin:secret".to_string());
    let addr = resolve_bind_addr(&cli).expect("0.0.0.0 parses");
    validate_bind(&cli, &addr).expect("non-loopback with auth should be allowed");
}

#[test]
fn non_loopback_bind_with_unsafe_flag_is_allowed() {
    let mut cli = minimal_cli();
    cli.bind_addr = Some("0.0.0.0".to_string());
    cli.unsafe_bind = true;
    let addr = resolve_bind_addr(&cli).expect("0.0.0.0 parses");
    validate_bind(&cli, &addr).expect("--unsafe-bind overrides the guard");
}

#[test]
fn loopback_bind_without_auth_is_allowed() {
    let mut cli = minimal_cli();
    cli.bind_addr = Some("127.0.0.1".to_string());
    let addr = resolve_bind_addr(&cli).expect("127.0.0.1 parses");
    assert!(is_loopback(&addr));
    validate_bind(&cli, &addr).expect("loopback without auth is the safe default");
}

#[test]
fn ipv6_loopback_is_loopback() {
    let mut cli = minimal_cli();
    cli.bind_addr = Some("::1".to_string());
    let addr = resolve_bind_addr(&cli).expect("::1 parses");
    assert!(is_loopback(&addr), "::1 is loopback");
    validate_bind(&cli, &addr).expect("ipv6 loopback is allowed without auth");
}

#[test]
fn invalid_bind_address_errors() {
    let mut cli = minimal_cli();
    cli.bind_addr = Some("not-an-address".to_string());
    let err = resolve_bind_addr(&cli).expect_err("garbage should not parse");
    assert!(err.contains("invalid bind address"));
}
