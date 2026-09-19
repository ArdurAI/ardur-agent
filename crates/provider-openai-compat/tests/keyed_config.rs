//! The keyed env-resolving constructor credential pools use (#411): a pooled
//! key must land on the SAME endpoint and timeout a direct `from_env`
//! selection would use — never silently on the public OpenAI default.

use ardur_provider_openai_compat::OpenAiCompatConfig;

#[test]
fn from_env_with_key_keeps_endpoint_and_timeout_env() {
    // Sole env-mutating test in this binary, so plain save/restore is enough.
    let saved: Vec<(&str, Option<String>)> = [
        ardur_provider_openai_compat::BASE_URL_ENV,
        ardur_provider_openai_compat::TIMEOUT_SECS_ENV,
    ]
    .iter()
    .map(|k| (*k, std::env::var(k).ok()))
    .collect();
    unsafe {
        std::env::set_var(
            ardur_provider_openai_compat::BASE_URL_ENV,
            "http://127.0.0.1:9/v1",
        );
        std::env::set_var(ardur_provider_openai_compat::TIMEOUT_SECS_ENV, "7");
    }
    let config = OpenAiCompatConfig::from_env_with_key("sk-pooled-fixture")
        .expect("valid loopback endpoint builds");
    let debug = format!("{config:?}");
    assert!(debug.contains("127.0.0.1:9"), "{debug}");
    assert!(debug.contains("7s"), "{debug}");
    assert!(!debug.contains("sk-pooled-fixture"), "key leaked: {debug}");

    // And the base-URL validator still fires through the keyed path.
    unsafe {
        std::env::set_var(
            ardur_provider_openai_compat::BASE_URL_ENV,
            "http://example.com/v1",
        );
    }
    assert!(
        OpenAiCompatConfig::from_env_with_key("sk-pooled-fixture").is_err(),
        "non-loopback HTTP must stay rejected"
    );

    for (key, value) in saved {
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}
