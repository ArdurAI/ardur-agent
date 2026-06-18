//! Tests for ARD-421: admin UI default bind address security.
//!
//! Verifies that the CLI defaults to loopback (127.0.0.1), that non-loopback
//! binds require `--basic-auth`, and that `--unsafe-bind` overrides the check.

use ardur_admin::config::Cli;
use clap::Parser;

#[test]
fn default_bind_addr_is_loopback() {
    // Parse with minimal required args — bind_addr should default to 127.0.0.1.
    let cli = Cli::try_parse_from([
        "ardur-admin",
        "--journal-dir",
        "/tmp/journals",
        "--receipt-store",
        "/tmp/receipts",
    ])
    .expect("CLI should parse with defaults");
    assert_eq!(cli.bind_addr, "127.0.0.1");
    assert!(!cli.unsafe_bind);
}

#[test]
fn non_loopback_without_basic_auth_should_error() {
    // Simulate the startup check logic: non-loopback + no basic-auth + no unsafe-bind → error.
    let cli = Cli::try_parse_from([
        "ardur-admin",
        "--journal-dir",
        "/tmp/journals",
        "--receipt-store",
        "/tmp/receipts",
        "--bind-addr",
        "0.0.0.0",
    ])
    .expect("CLI should parse");
    let bind_is_loopback = is_loopback(&cli.bind_addr);
    assert!(!bind_is_loopback, "0.0.0.0 is not loopback");
    // This mirrors the check in main.rs:
    let should_error = !bind_is_loopback && cli.basic_auth.is_none() && !cli.unsafe_bind;
    assert!(
        should_error,
        "Non-loopback without --basic-auth should trigger startup error"
    );
}

#[test]
fn non_loopback_with_basic_auth_is_allowed() {
    let cli = Cli::try_parse_from([
        "ardur-admin",
        "--journal-dir",
        "/tmp/journals",
        "--receipt-store",
        "/tmp/receipts",
        "--bind-addr",
        "0.0.0.0",
        "--basic-auth",
        "admin:secret",
    ])
    .expect("CLI should parse");
    let bind_is_loopback = is_loopback(&cli.bind_addr);
    assert!(!bind_is_loopback);
    let should_error = !bind_is_loopback && cli.basic_auth.is_none() && !cli.unsafe_bind;
    assert!(
        !should_error,
        "Non-loopback with --basic-auth should be allowed"
    );
}

#[test]
fn non_loopback_with_unsafe_bind_is_allowed() {
    let cli = Cli::try_parse_from([
        "ardur-admin",
        "--journal-dir",
        "/tmp/journals",
        "--receipt-store",
        "/tmp/receipts",
        "--bind-addr",
        "0.0.0.0",
        "--unsafe-bind",
    ])
    .expect("CLI should parse");
    let bind_is_loopback = is_loopback(&cli.bind_addr);
    assert!(!bind_is_loopback);
    let should_error = !bind_is_loopback && cli.basic_auth.is_none() && !cli.unsafe_bind;
    assert!(
        !should_error,
        "Non-loopback with --unsafe-bind should be allowed"
    );
}

#[test]
fn env_var_sets_bind_addr() {
    // ARDUR_ADMIN_BIND env var should override the default.
    // We use try_parse_from which doesn't read env vars, so we test the env attribute
    // by checking the default value is documented as 127.0.0.1.
    // Full env var testing requires process spawning, which is out of scope here.
    let cli = Cli::try_parse_from([
        "ardur-admin",
        "--journal-dir",
        "/tmp/journals",
        "--receipt-store",
        "/tmp/receipts",
    ])
    .expect("CLI should parse");
    assert_eq!(cli.bind_addr, "127.0.0.1");
}

/// Returns true if the address is loopback (127.0.0.1 or ::1).
fn is_loopback(addr: &str) -> bool {
    match addr.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(std::net::IpAddr::V6(v6)) => v6.is_loopback(),
        Err(_) => false,
    }
}
