//! The `[router]` config table — typed parsing for the model-router substrate
//! (#411 / #531 D0).
//!
//! The table lives in the operator's `~/.ardur/config.toml` next to the CLI's
//! flat keys and the `[integrations.*]` sections. It is the *only* router
//! configuration surface; the environment contributes the kill-switch
//! (`ARDUR_ROUTER=off`) and credential pools (`ARDUR_<PROVIDER>_KEYS`), both
//! consumed by `ardur-provider-selector` when it builds the router.
//!
//! ```toml
//! [router]
//! default = "standard"
//!
//! [[router.lanes.standard]]
//! backend = "anthropic"
//! model = "claude-opus-4-8"
//!
//! [[router.lanes.standard]]
//! backend = "ollama"
//! model = "llama3.3"
//!
//! [router.model_overrides."claude-opus-4-8"]
//! input_price_micros = 3000
//! output_price_micros = 15000
//!
//! [router.credential_pools]
//! anthropic = ["sk-ant-...a", "sk-ant-...b"]
//! ```
//!
//! Every struct denies unknown fields: a typo'd key is a boot error, not a
//! silently dropped setting.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::ConfigError;

/// One pool credential. The key material is *never* printed: [`Debug`] renders
/// the `[redacted]` marker, [`Display`] is intentionally not implemented, and
/// [`Serialize`] is deliberately unimplemented so a pooled key cannot leak
/// into a serialized document at compile time. The value is reachable only
/// through [`PooledKey::expose`] at the one call site that injects it into a
/// provider constructor.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct PooledKey(String);

/// The marker every redacted rendering of a [`PooledKey`] carries. Tests pin
/// it in both directions: the marker must appear, the key value must not.
pub const REDACTION_MARKER: &str = "[redacted]";

impl PooledKey {
    /// Borrow the secret value. The only legitimate callers are provider
    /// constructors that need the key material itself.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PooledKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTION_MARKER)
    }
}

/// One ordered entry in a lane's failover chain: the backend selector spelling
/// (`anthropic`, `openrouter`, `openai-compat`, `ollama`, `codex`,
/// `claude-cli`, `prime`) and the model completions are pinned to on it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneEntry {
    /// The provider backend, spelled as in `ARDUR_PROVIDER`.
    pub backend: String,
    /// The model id this entry runs completions against.
    pub model: String,
}

/// Operator-supplied metadata patch for one model id (#411 `model_overrides`).
///
/// Prices are stated in *micro-cents* (millionths of a US cent) per 1,000
/// tokens so the table stays integer-only; the router converts to the
/// fractional-cent floats the [`RateCard`](ardur_provider_runtime) speaks. A
/// field left absent keeps the provider's own published value — the override
/// table never invents defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOverride {
    /// The model's total context window in tokens, when the operator knows it
    /// differs from (or is absent from) the provider's published metadata.
    /// Parsed and carried, but no runtime consumer exists yet — it is exposed
    /// for the routing/context lanes that land after D0.
    pub context_window: Option<u64>,
    /// Price per 1,000 input tokens, in millionths of a US cent.
    pub input_price_micros: Option<u64>,
    /// Price per 1,000 output tokens, in millionths of a US cent.
    pub output_price_micros: Option<u64>,
}

/// The parsed `[router]` table.
///
/// `default` names the lane requests fall back to when their task class
/// matches no configured lane; a `[router]` table without it is a boot error,
/// because an unstated fallback is exactly the misconfiguration that silently
/// disables routing. Lanes with zero entries are rejected later, at router
/// construction, where the error can name the lane.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterTable {
    /// Name of the fallback lane; must exist as a key in [`lanes`](Self::lanes).
    pub default: String,
    /// Task class -> ordered failover chain.
    pub lanes: HashMap<String, Vec<LaneEntry>>,
    /// Model id -> operator metadata patch.
    #[serde(default)]
    pub model_overrides: HashMap<String, ModelOverride>,
    /// Backend spelling -> pooled credentials. The environment twin is
    /// `ARDUR_<PROVIDER>_KEYS`; the environment wins when both are set.
    #[serde(default)]
    pub credential_pools: HashMap<String, Vec<PooledKey>>,
}

/// The default config path: `~/.ardur/config.toml`. Mirrors the CLI's own
/// convention (`crates/cli/src/config.rs`); the router has no access to the
/// CLI's `--config` flag, so a config file at a custom path does not feed the
/// router in D0 (documented in RUN.md). `None` when no home directory can be
/// resolved.
#[must_use]
pub fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".ardur").join("config.toml"))
}

/// Extract the `[router]` table from config TOML text.
///
/// Returns `Ok(None)` when the document has no `[router]` table — the common
/// case, which must reproduce today's single-provider boot byte-for-byte. A
/// document that does not parse as TOML also yields `Ok(None)` *with a loud
/// warning*: the CLI's historical flat reader tolerates files a strict parser
/// rejects, and aborting boot here would break operators who never asked for
/// a router. The exception is a document that *declares router intent* (a
/// `[router]`/`[router.*]` header line) but fails to parse: that is a broken
/// routing configuration, not a router-free file, and boot aborts with a
/// typed error rather than silently running without the intended failover. A
/// present-but-malformed `[router]` table is likewise a typed
/// [`ConfigError::InvalidValue`].
///
/// # Errors
///
/// Returns [`ConfigError::InvalidValue`] when a present `[router]` table fails
/// schema parsing (unknown fields, wrong types, missing `default`/`lanes`),
/// when a router-declaring document is not valid TOML, or when a
/// `credential_pools` value is not an array — that last message is sanitized
/// to name the backend only, since the deserializer's own error would quote
/// the malformed key value.
pub fn router_table_from_str(contents: &str) -> Result<Option<RouterTable>, ConfigError> {
    let document: toml::Value = match toml::from_str(contents) {
        Ok(document) => document,
        Err(e) => {
            if declares_router_intent(contents) {
                return Err(ConfigError::InvalidValue(format!(
                    "config declares a [router] table but the file is not valid TOML: {e}"
                )));
            }
            tracing::warn!(
                error = %e,
                "config file is not valid TOML; ignoring any [router] table and \
                 continuing with the single-provider path"
            );
            return Ok(None);
        }
    };
    let Some(table) = document.get("router") else {
        return Ok(None);
    };
    // Sanitize pool shape BEFORE the schema parse: a scalar pool value
    // (`anthropic = "sk-..."`) makes the deserializer's `invalid type`
    // message quote the key material verbatim. Reject it here naming only
    // the backend.
    if let Some(pools) = table
        .get("credential_pools")
        .and_then(toml::Value::as_table)
    {
        for (backend, value) in pools {
            if !value.is_array() {
                return Err(ConfigError::InvalidValue(format!(
                    "[router].credential_pools.{backend} must be an array of key strings"
                )));
            }
        }
    }
    let parsed: RouterTable = table
        .clone()
        .try_into()
        .map_err(|e| ConfigError::InvalidValue(format!("invalid [router] table in config: {e}")))?;
    Ok(Some(parsed))
}

/// Does the raw text declare router intent? A comment-stripped line whose
/// trim starts with a `[router]` or `[router.*]` header. Only consulted when
/// strict TOML parsing has already failed, to distinguish "file the flat
/// reader tolerates" from "broken routing configuration".
fn declares_router_intent(contents: &str) -> bool {
    contents.lines().any(|raw| {
        let line = raw.split('#').next().unwrap_or("").trim();
        line.starts_with("[router]") || line.starts_with("[router.")
    })
}

/// Load the `[router]` table from a config file on disk.
///
/// A missing file is `Ok(None)` — a fresh checkout has no config and boots as
/// before. An unreadable file (permissions, I/O) is a typed error, because
/// silently skipping a file the operator can see would hide configuration.
///
/// # Errors
///
/// Returns [`ConfigError::Io`] when the file exists but cannot be read, and
/// [`ConfigError::InvalidValue`] for a present-but-malformed `[router]` table.
pub fn load_router_table(path: &Path) -> Result<Option<RouterTable>, ConfigError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ConfigError::Io(e)),
    };
    router_table_from_str(&contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_TABLE: &str = r#"
api_key = "cli-flat-key"
model = "claude-opus-4-8"

[integrations.beads]
enabled = true

[router]
default = "standard"

[[router.lanes.standard]]
backend = "anthropic"
model = "claude-opus-4-8"

[[router.lanes.standard]]
backend = "ollama"
model = "llama3.3"

[[router.lanes.code]]
backend = "openrouter"
model = "qwen/qwen3-coder"

[router.model_overrides."claude-opus-4-8"]
context_window = 200000
input_price_micros = 3000
output_price_micros = 15000

[router.credential_pools]
anthropic = ["sk-ant-alpha", "sk-ant-beta"]
"#;

    #[test]
    fn router_table_parses_full_surface() {
        let table = router_table_from_str(FULL_TABLE)
            .expect("valid table parses")
            .expect("table is present");
        assert_eq!(table.default, "standard");
        assert_eq!(table.lanes.len(), 2);
        let standard = &table.lanes["standard"];
        assert_eq!(standard.len(), 2);
        assert_eq!(standard[0].backend, "anthropic");
        assert_eq!(standard[0].model, "claude-opus-4-8");
        assert_eq!(standard[1].backend, "ollama");
        assert_eq!(table.lanes["code"][0].backend, "openrouter");
        let patch = &table.model_overrides["claude-opus-4-8"];
        assert_eq!(patch.context_window, Some(200000));
        assert_eq!(patch.input_price_micros, Some(3000));
        assert_eq!(patch.output_price_micros, Some(15000));
        let pool = &table.credential_pools["anthropic"];
        assert_eq!(pool.len(), 2);
        assert_eq!(pool[0].expose(), "sk-ant-alpha");
        assert_eq!(pool[1].expose(), "sk-ant-beta");
    }

    #[test]
    fn router_table_absent_when_no_router_key() {
        let doc = "model = \"claude-opus-4-8\"\n[integrations.beads]\nenabled = true\n";
        assert_eq!(
            router_table_from_str(doc).expect("parse ok"),
            None,
            "a config without [router] must yield None (today's boot path)"
        );
    }

    #[test]
    fn router_table_absent_when_document_is_not_toml() {
        // The CLI's flat reader tolerates files a strict TOML parser rejects;
        // the router probe must not break those operators' boots.
        assert_eq!(
            router_table_from_str("this is = not = toml = at all = [[[").expect("tolerated"),
            None
        );
    }

    #[test]
    fn router_declaring_document_with_syntax_errors_aborts_boot() {
        // An explicit [router] section with a TOML syntax error (here an
        // unterminated model string) is a BROKEN ROUTING CONFIGURATION, not
        // a router-free file: boot must fail loudly, never silently take the
        // single-provider path and bypass the intended failover.
        let doc = "[router]\ndefault = \"standard\"\n\n[[router.lanes.standard]]\nbackend = \"ollama\"\nmodel = \"llama3.3\n";
        let err =
            router_table_from_str(doc).expect_err("router intent + syntax error = boot error");
        let msg = err.to_string();
        assert!(msg.contains("[router]"), "{msg}");
        assert!(msg.contains("not valid TOML"), "{msg}");
    }

    #[test]
    fn commented_out_router_header_is_not_intent() {
        // A commented header in an otherwise unparseable file stays on the
        // tolerant path — the operator did not declare routing.
        assert_eq!(
            router_table_from_str("# [router]\nthis is = [[[").expect("tolerated"),
            None
        );
    }

    #[test]
    fn scalar_credential_pool_is_rejected_without_quoting_the_key() {
        let fixture = "sk-scalar-FIXTURE-77c1-secret";
        let doc = format!(
            "[router]\ndefault = \"standard\"\nlanes = {{ standard = [ {{ backend = \"anthropic\", model = \"m\" }} ] }}\n\
             [router.credential_pools]\nanthropic = \"{fixture}\"\n"
        );
        let err = router_table_from_str(&doc).expect_err("scalar pool must error");
        let msg = err.to_string();
        // Must-match: the backend is named so the operator can find the key.
        assert!(msg.contains("anthropic"), "{msg}");
        assert!(msg.contains("array"), "{msg}");
        // Must-NOT-match: the malformed VALUE never enters the message
        // (toml's own invalid-type error would quote it verbatim).
        assert!(!msg.contains(fixture), "key material leaked: {msg}");
        assert!(!msg.contains("sk-scalar"), "key prefix leaked: {msg}");
    }

    #[test]
    fn pool_array_with_a_non_string_element_does_not_leak_siblings() {
        let fixture = "sk-sibling-FIXTURE-991e-secret";
        let doc = format!(
            "[router]\ndefault = \"standard\"\nlanes = {{ standard = [ {{ backend = \"anthropic\", model = \"m\" }} ] }}\n\
             [router.credential_pools]\nanthropic = [ \"{fixture}\", 42 ]\n"
        );
        let err = router_table_from_str(&doc).expect_err("non-string element must error");
        let msg = err.to_string();
        assert!(
            !msg.contains(fixture),
            "a sibling's key material leaked via the element error: {msg}"
        );
    }

    #[test]
    fn router_table_denies_unknown_fields() {
        let doc = r#"
[router]
default = "standard"
dfault_lanes_typo = true

[router.lanes]
standard = []
"#;
        let err = router_table_from_str(doc)
            .expect_err("an unknown field in [router] must be a typed error");
        let msg = err.to_string();
        assert!(msg.contains("invalid [router] table"), "{msg}");
        assert!(msg.contains("dfault_lanes_typo"), "{msg}");
    }

    #[test]
    fn router_table_requires_default_lane_name() {
        let doc = r#"
[router]
lanes = {}
"#;
        let err = router_table_from_str(doc).expect_err("missing `default` must error");
        assert!(err.to_string().contains("default"), "{err}");
    }

    #[test]
    fn lane_entry_denies_unknown_fields() {
        let doc = r#"
[router]
default = "standard"
lanes = { standard = [ { backend = "ollama", model = "llama3.3", region = "moon" } ] }
"#;
        let err = router_table_from_str(doc).expect_err("unknown lane-entry field must error");
        assert!(err.to_string().contains("region"), "{err}");
    }

    #[test]
    fn load_router_table_missing_file_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.toml");
        assert_eq!(
            load_router_table(&missing).expect("missing tolerated"),
            None
        );
    }

    #[test]
    fn load_router_table_reads_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, FULL_TABLE).expect("write fixture");
        let table = load_router_table(&path).expect("loads").expect("present");
        assert_eq!(table.default, "standard");
    }

    #[test]
    fn pooled_key_debug_is_redacted_both_directions() {
        let fixture = "sk-test-FIXTURE-aa91d4-secret-value";
        let key = PooledKey(fixture.to_string());
        let debug = format!("{key:?}");
        // Must-match: the redaction marker is rendered.
        assert!(
            debug.contains(REDACTION_MARKER),
            "redaction marker missing from Debug output: {debug}"
        );
        // Must-NOT-match: no substring of the fixture key leaks.
        assert!(
            !debug.contains(fixture),
            "key material leaked into Debug output: {debug}"
        );
        // And the value is still reachable for the one legitimate consumer.
        assert_eq!(key.expose(), fixture);
    }

    #[test]
    fn pooled_keys_stay_redacted_inside_router_table_debug() {
        let fixture = "sk-ant-FIXTURE-bb77e0-secret-value";
        let doc = format!(
            "[router]\ndefault = \"standard\"\nlanes = {{ standard = [ {{ backend = \"anthropic\", model = \"m\" }} ] }}\n\
             credential_pools = {{ anthropic = [ \"{fixture}\" ] }}\n"
        );
        let table = router_table_from_str(&doc)
            .expect("parses")
            .expect("present");
        let debug = format!("{table:?}");
        assert!(debug.contains(REDACTION_MARKER), "{debug}");
        assert!(!debug.contains(fixture), "pool key leaked: {debug}");
    }
}
