//! Shared fixtures for the §4.1 Slack-adapter integration tests: deterministic
//! credentials, a builder for the adapter, and a helper that recomputes a
//! genuine `v0=` request signature so the verify path can be exercised against
//! real HMACs rather than hand-rolled hex.

#![allow(dead_code)]

use hmac::{Hmac, Mac};
use secrecy::SecretString;
use sha2::Sha256;

use ardur_slack_adapter::SlackAdapter;

type HmacSha256 = Hmac<Sha256>;

/// A throwaway signing secret — never a real one.
pub const SIGNING_SECRET: &str = "8f742231b10e8888abcd99eeffaa1234";
/// A throwaway bot token.
pub const BOT_TOKEN: &str = "xoxb-0000-test-token";
/// The app id the adapter is configured for (used for bot-own filtering and the
/// gateway channel namespace).
pub const APP_ID: &str = "A0123TESTAPP";

/// A fixed "now" (Unix seconds) the deterministic `parse_event_at` checks
/// against — well inside the replay window for fresh-timestamp fixtures.
pub const NOW_UNIX: u64 = 1_750_000_000;

/// Build an adapter on the deterministic test credentials. `base_url` repoints
/// the Web-API host (pass a wiremock `uri()` for send tests); pass `None` for
/// the inbound-only tests that never make an HTTP call.
pub fn test_adapter(base_url: Option<String>) -> SlackAdapter {
    let adapter = SlackAdapter::new(
        SecretString::new(BOT_TOKEN.to_string()),
        SecretString::new(SIGNING_SECRET.to_string()),
        APP_ID.to_string(),
    );
    match base_url {
        Some(url) => adapter.with_base_url(url),
        None => adapter,
    }
}

/// Recompute the genuine Slack `v0=<hex>` signature over the request basestring
/// — the same construction the adapter verifies against.
pub fn sign(timestamp: &str, body: &str) -> String {
    let basestring = format!("v0:{timestamp}:{body}");
    let mut mac =
        HmacSha256::new_from_slice(SIGNING_SECRET.as_bytes()).expect("hmac accepts any key length");
    mac.update(basestring.as_bytes());
    format!("v0={}", hex::encode(mac.finalize().into_bytes()))
}
