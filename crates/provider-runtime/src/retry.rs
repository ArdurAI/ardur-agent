//! Shared `Retry-After` response-header parsing.

use std::time::SystemTime;

use reqwest::header::{HeaderMap, RETRY_AFTER};

/// Parse the `Retry-After` header into a backoff delay in milliseconds,
/// defaulting to `0` when the header is absent or unparseable.
///
/// RFC 7231 §7.1.3 permits **two** forms:
/// - `delta-seconds` — a non-negative decimal count of seconds; and
/// - `HTTP-date` — an absolute time (e.g. `Wed, 21 Oct 2026 07:28:00 GMT`).
///
/// Handling only the first meant a rate-limited response that used the date form
/// fell through to `0` (ARD-M6), so a backoff layer would retry *immediately*
/// back into the 429 — a hot loop against the provider. This resolves the date
/// form against the current clock; a date already in the past clamps to `0`
/// (retry now), and an absurdly-distant date saturates rather than overflowing.
pub fn parse_retry_after_ms(headers: &HeaderMap) -> u64 {
    let Some(value) = headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
    else {
        return 0;
    };

    // delta-seconds (the common case).
    if let Ok(secs) = value.parse::<u64>() {
        return secs.saturating_mul(1000);
    }

    // HTTP-date: back-compute the delay from now, clamping a past date to 0.
    if let Ok(when) = httpdate::parse_http_date(value) {
        return when
            .duration_since(SystemTime::now())
            .map(|delay| u64::try_from(delay.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
    }

    0
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn headers_with(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, value.parse().expect("valid header value"));
        headers
    }

    #[test]
    fn delta_seconds_becomes_milliseconds() {
        assert_eq!(parse_retry_after_ms(&headers_with("2")), 2000);
        assert_eq!(parse_retry_after_ms(&headers_with("  5 ")), 5000);
    }

    #[test]
    fn absent_or_garbage_header_is_zero() {
        assert_eq!(parse_retry_after_ms(&HeaderMap::new()), 0);
        assert_eq!(parse_retry_after_ms(&headers_with("soon")), 0);
        assert_eq!(parse_retry_after_ms(&headers_with("1.5")), 0);
    }

    #[test]
    fn http_date_in_the_future_yields_a_positive_delay() {
        // A date ~1 hour out must produce a delay close to (but at most) 1 hour,
        // rather than the 0 the delta-seconds-only parser returned.
        let future = SystemTime::now() + Duration::from_secs(3600);
        let ms = parse_retry_after_ms(&headers_with(&httpdate::fmt_http_date(future)));
        // HTTP-date has whole-second resolution, so allow a small lower slack.
        assert!(
            (3_590_000..=3_600_000).contains(&ms),
            "expected ~3.6e6 ms, got {ms}"
        );
    }

    #[test]
    fn http_date_in_the_past_clamps_to_zero() {
        let past = SystemTime::now() - Duration::from_secs(3600);
        assert_eq!(
            parse_retry_after_ms(&headers_with(&httpdate::fmt_http_date(past))),
            0
        );
    }
}
