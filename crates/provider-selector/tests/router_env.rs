//! Boot-wiring evidence for the router branch of `provider_selector::from_env`
//! (#411 / #531 D0): kill-switch, absent-table byte-identical path, table
//! build, fail-closed config errors, pool expansion, and key redaction.
//!
//! Every test mutates process-global environment (`HOME`, `ARDUR_*`), so the
//! file runs fully serial and restores each variable on drop.

use std::path::Path;

use ardur_provider_selector::router_build::{ROUTER_ENV, build_router_provider};
use ardur_provider_selector::{ModelId, from_env};
use serial_test::serial;

/// Every environment variable these tests may set — cleared before each one
/// so ambient developer/CI env cannot leak in.
const MANAGED_ENV: &[&str] = &[
    "HOME",
    "ARDUR_PROVIDER",
    ROUTER_ENV,
    "ARDUR_ANTHROPIC_KEYS",
    "ARDUR_OPENROUTER_KEYS",
    "ARDUR_OLLAMA_KEYS",
    "ANTHROPIC_API_KEY",
    "OPENROUTER_API_KEY",
    "OPENAI_COMPAT_API_KEY",
    "OPENAI_API_KEY",
    "OPENAI_COMPAT_BASE_URL",
    "OPENAI_COMPAT_TIMEOUT_SECS",
];

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn fresh() -> (Self, tempfile::TempDir) {
        let saved: Vec<(&'static str, Option<String>)> = MANAGED_ENV
            .iter()
            .map(|k| (*k, std::env::var(k).ok()))
            .collect();
        for k in MANAGED_ENV {
            unsafe { std::env::remove_var(k) };
        }
        let home = tempfile::tempdir().expect("temp home");
        unsafe { std::env::set_var("HOME", home.path()) };
        (Self { saved }, home)
    }

    fn set(key: &str, value: &str) {
        unsafe { std::env::set_var(key, value) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }
}

/// Write a config file holding `body` into the scratch HOME.
fn write_config(home: &Path, body: &str) {
    let dir = home.join(".ardur");
    std::fs::create_dir_all(&dir).expect("config dir");
    std::fs::write(dir.join("config.toml"), body).expect("config file");
}

const OLLAMA_ROUTER_TABLE: &str = r#"
[router]
default = "standard"

[[router.lanes.standard]]
backend = "ollama"
model = "llama3.3"
"#;

#[test]
#[serial]
fn absent_table_keeps_the_single_provider_path_byte_identical() {
    let (_guard, _home) = EnvGuard::fresh();
    EnvGuard::set("ARDUR_PROVIDER", "ollama");
    // No config file at all: from_env must behave exactly as before the
    // router existed — an `ollama` provider, not a router.
    let provider = from_env(ModelId::new("test-model")).expect("ollama is infallible");
    assert_eq!(provider.id().0, "ollama");
}

#[test]
#[serial]
fn kill_switch_forces_single_provider_even_with_a_table() {
    let (_guard, home) = EnvGuard::fresh();
    write_config(home.path(), OLLAMA_ROUTER_TABLE);
    EnvGuard::set(ROUTER_ENV, "off");
    EnvGuard::set("ARDUR_PROVIDER", "ollama");
    let provider = from_env(ModelId::new("test-model")).expect("ollama is infallible");
    assert_eq!(
        provider.id().0,
        "ollama",
        "ARDUR_ROUTER=off bypasses the router"
    );
}

#[test]
#[serial]
fn kill_switch_accepts_case_and_zero_and_false() {
    for spelling in ["OFF", "0", "False"] {
        let (_guard, home) = EnvGuard::fresh();
        write_config(home.path(), OLLAMA_ROUTER_TABLE);
        EnvGuard::set(ROUTER_ENV, spelling);
        EnvGuard::set("ARDUR_PROVIDER", "ollama");
        let provider = from_env(ModelId::new("test-model")).expect("ollama");
        assert_eq!(
            provider.id().0,
            "ollama",
            "{spelling:?} must disable the router"
        );
    }
}

#[test]
#[serial]
fn configured_table_builds_the_router() {
    let (_guard, home) = EnvGuard::fresh();
    write_config(home.path(), OLLAMA_ROUTER_TABLE);
    let provider = from_env(ModelId::new("test-model")).expect("router builds");
    assert_eq!(provider.id().0, "router");
}

#[test]
#[serial]
fn ardur_provider_is_superseded_by_a_configured_table() {
    let (_guard, home) = EnvGuard::fresh();
    write_config(home.path(), OLLAMA_ROUTER_TABLE);
    EnvGuard::set("ARDUR_PROVIDER", "ollama");
    // The selector spelling is ignored (with a loud warning): the router owns
    // selection when its table exists.
    let provider = from_env(ModelId::new("test-model")).expect("router builds");
    assert_eq!(provider.id().0, "router");
}

#[test]
#[serial]
fn malformed_table_aborts_boot_with_a_typed_error() {
    let (_guard, home) = EnvGuard::fresh();
    write_config(
        home.path(),
        "[router]\ndefault = \"standard\"\nlanes = {}\nsurprise_key = 1\n",
    );
    let err = from_env(ModelId::new("test-model"))
        .err()
        .expect("must fail");
    let msg = err.to_string();
    assert!(msg.contains("surprise_key"), "{msg}");
    assert!(msg.contains("[router]"), "{msg}");
}

#[test]
#[serial]
fn missing_default_lane_aborts_boot_naming_the_lane() {
    let (_guard, home) = EnvGuard::fresh();
    write_config(
        home.path(),
        "[router]\ndefault = \"production\"\n\
         lanes = { staging = [ { backend = \"ollama\", model = \"m\" } ] }\n",
    );
    let err = from_env(ModelId::new("test-model"))
        .err()
        .expect("must fail");
    assert!(
        err.to_string().contains("production"),
        "the error names the missing default lane: {err}"
    );
}

#[test]
#[serial]
fn unknown_backend_aborts_boot_listing_supported_values() {
    let (_guard, home) = EnvGuard::fresh();
    write_config(
        home.path(),
        "[router]\ndefault = \"standard\"\n\
         lanes = { standard = [ { backend = \"mistral\", model = \"m\" } ] }\n",
    );
    let err = from_env(ModelId::new("test-model"))
        .err()
        .expect("must fail");
    assert!(err.to_string().contains("supported values"), "{err}");
}

#[test]
#[serial]
fn env_pool_expands_a_lane_entry_per_key() {
    let (_guard, home) = EnvGuard::fresh();
    write_config(
        home.path(),
        "[router]\ndefault = \"standard\"\n\
         lanes = { standard = [ { backend = \"anthropic\", model = \"claude-opus-4-8\" } ] }\n",
    );
    EnvGuard::set("ARDUR_ANTHROPIC_KEYS", "sk-one, sk-two ,sk-three");
    let provider = from_env(ModelId::new("test-model")).expect("router builds");
    assert_eq!(provider.id().0, "router");
    // Inspect the concrete router: three entries from one configured line.
    let table = ardur_config::load_router_table(&home.path().join(".ardur/config.toml"))
        .expect("loads")
        .expect("present");
    let router = build_router_provider(&table).expect("builds");
    assert_eq!(router.lane_len("standard"), Some(3));
    assert_eq!(router.lane_count(), 1);
}

#[test]
#[serial]
fn config_pool_is_used_when_env_is_absent_and_env_wins_when_both_exist() {
    let (_guard, home) = EnvGuard::fresh();
    write_config(
        home.path(),
        "[router]\ndefault = \"standard\"\n\
         lanes = { standard = [ { backend = \"anthropic\", model = \"m\" } ] }\n\
         [router.credential_pools]\nanthropic = [ \"sk-config-a\", \"sk-config-b\" ]\n",
    );
    let table = ardur_config::load_router_table(&home.path().join(".ardur/config.toml"))
        .expect("loads")
        .expect("present");
    let router = build_router_provider(&table).expect("builds");
    assert_eq!(router.lane_len("standard"), Some(2), "config pool used");

    EnvGuard::set("ARDUR_ANTHROPIC_KEYS", "sk-env-1,sk-env-2,sk-env-3");
    let router = build_router_provider(&table).expect("builds");
    assert_eq!(router.lane_len("standard"), Some(3), "env pool wins");
}

#[test]
#[serial]
fn unpooled_credentialed_backend_keeps_todays_single_key_behavior() {
    let (_guard, home) = EnvGuard::fresh();
    write_config(
        home.path(),
        "[router]\ndefault = \"standard\"\n\
         lanes = { standard = [ { backend = \"anthropic\", model = \"m\" } ] }\n",
    );
    // With no pool anywhere and no ambient key, the same Unauthorized a
    // direct selection would raise aborts boot — byte-identical semantics.
    let err = from_env(ModelId::new("test-model"))
        .err()
        .expect("must fail");
    assert!(
        matches!(err, ardur_provider_selector::ProviderError::Unauthorized),
        "{err:?}"
    );
    // With the ambient key present, exactly one entry is built.
    EnvGuard::set("ANTHROPIC_API_KEY", "sk-ambient");
    let provider = from_env(ModelId::new("test-model")).expect("builds");
    assert_eq!(provider.id().0, "router");
}

#[test]
#[serial]
fn pool_key_values_never_appear_in_boot_errors_or_debug_surfaces() {
    let (_guard, home) = EnvGuard::fresh();
    let fixture = "sk-FIXTURE-cc19d0-secret-material";
    write_config(
        home.path(),
        &format!(
            "[router]\ndefault = \"production\"\n\
             lanes = {{ staging = [ {{ backend = \"mistral\", model = \"m\" }} ] }}\n\
             [router.credential_pools]\nmistral = [ \"{fixture}\" ]\n"
        ),
    );
    // Two stacked errors here: the missing default lane AND the unknown
    // backend. Neither may carry the key material.
    let err = from_env(ModelId::new("test-model"))
        .err()
        .expect("must fail");
    let msg = format!("{err:?} {err}");
    assert!(
        !msg.contains(fixture),
        "pool key material leaked into the boot error: {msg}"
    );
    // Must-match direction: the redaction marker is what a pooled key renders.
    let table = ardur_config::load_router_table(&home.path().join(".ardur/config.toml"))
        .expect("loads")
        .expect("present");
    let debug = format!("{table:?}");
    assert!(
        debug.contains(ardur_config::REDACTION_MARKER),
        "redaction marker missing: {debug}"
    );
    assert!(
        !debug.contains(fixture),
        "key leaked via table Debug: {debug}"
    );
}

#[test]
#[serial]
fn model_overrides_flow_from_the_table_into_the_router() {
    let (_guard, home) = EnvGuard::fresh();
    write_config(
        home.path(),
        "[router]\ndefault = \"standard\"\n\
         lanes = { standard = [ { backend = \"ollama\", model = \"llama3.3\" } ] }\n\
         [router.model_overrides.\"llama3.3\"]\ncontext_window = 131072\n",
    );
    let table = ardur_config::load_router_table(&home.path().join(".ardur/config.toml"))
        .expect("loads")
        .expect("present");
    let router = build_router_provider(&table).expect("builds");
    assert_eq!(
        router.context_window(&ModelId::new("llama3.3")),
        Some(131_072)
    );
    assert!(router.unknown_override_models().is_empty());
}

#[test]
#[serial]
fn empty_lanes_map_aborts_boot() {
    let (_guard, home) = EnvGuard::fresh();
    write_config(
        home.path(),
        "[router]\ndefault = \"standard\"\nlanes = {}\n",
    );
    let err = from_env(ModelId::new("test-model"))
        .err()
        .expect("must fail");
    assert!(err.to_string().contains("standard"), "{err}");
}

#[test]
#[serial]
fn pooled_openai_compat_entries_keep_the_operators_endpoint_env() {
    // A pooled key must land on the SAME service a direct selection would
    // use. The sharpest observable proof is the validator: a non-loopback
    // HTTP base URL is rejected at build time — if the pooled path built its
    // config with plain `new(key)` (dropping env), no error would occur and
    // the key would head for the public OpenAI default instead.
    let (_guard, home) = EnvGuard::fresh();
    write_config(
        home.path(),
        "[router]\ndefault = \"standard\"\n\
         lanes = { standard = [ { backend = \"openai-compat\", model = \"gpt-test\" } ] }\n",
    );
    EnvGuard::set("ARDUR_OPENAI_COMPAT_KEYS", "sk-pooled-1,sk-pooled-2");
    EnvGuard::set("OPENAI_COMPAT_BASE_URL", "http://example.com/v1");
    let err = from_env(ModelId::new("test-model"))
        .err()
        .expect("non-loopback HTTP base URL must fail validation");
    assert!(err.to_string().contains("OPENAI_COMPAT_BASE_URL"), "{err}");

    // And the valid form builds: loopback HTTP is the documented local-test
    // exception, so the pool constructs and expands.
    EnvGuard::set("OPENAI_COMPAT_BASE_URL", "http://127.0.0.1:9/v1");
    EnvGuard::set("OPENAI_COMPAT_TIMEOUT_SECS", "7");
    let table = ardur_config::load_router_table(&home.path().join(".ardur/config.toml"))
        .expect("loads")
        .expect("present");
    let router = build_router_provider(&table).expect("builds with loopback endpoint");
    assert_eq!(router.lane_len("standard"), Some(2));
}

#[test]
#[serial]
fn duplicate_pool_keys_are_deduplicated_before_indexing() {
    // The same key twice is ONE credential: indexing both would let an
    // `Unauthorized` "rotate" onto the very key that was just rejected.
    let (_guard, home) = EnvGuard::fresh();
    write_config(
        home.path(),
        "[router]\ndefault = \"standard\"\n\
         lanes = { standard = [ { backend = \"anthropic\", model = \"m\" } ] }\n",
    );
    EnvGuard::set("ARDUR_ANTHROPIC_KEYS", "sk-same,sk-same,sk-other");
    let table = ardur_config::load_router_table(&home.path().join(".ardur/config.toml"))
        .expect("loads")
        .expect("present");
    let router = build_router_provider(&table).expect("builds");
    assert_eq!(
        router.lane_len("standard"),
        Some(2),
        "two DISTINCT keys, not three slots"
    );
}
