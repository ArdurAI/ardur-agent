//! Building the D0 model router from configuration (#411 / #531).
//!
//! This module is the only place that can construct a [`RouterProvider`]:
//! it sits above every concrete backend crate (so it can build them) and
//! below the boot entry point [`crate::from_env`]. The data flow is:
//!
//! 1. `~/.ardur/config.toml` → [`ardur_config::RouterTable`] (lanes,
//!    overrides, pools) — absent table means no router, byte-identical boot.
//! 2. `ARDUR_<PROVIDER>_KEYS` / `[router.credential_pools]` → one provider
//!    instance per pooled key, adjacent in the lane's chain (a backend's
//!    credentials are exhausted before failing over to the next backend).
//! 3. `[router.model_overrides]` → [`ModelOverrideSpec`]s the router applies
//!    to the entries' effective rate cards.
//!
//! Precedence, fail-closed everywhere:
//!
//! - `ARDUR_ROUTER=off` (also `0`/`false`) disables the router even when a
//!   table is present — the operator's kill-switch back to today's path.
//! - A present `[router]` table makes the router own provider selection;
//!   a set `ARDUR_PROVIDER` is superseded with a loud warning (the
//!   kill-switch is the documented escape, not two competing selections).
//! - Every configuration error (unknown backend, missing default lane,
//!   unreadable config file) aborts boot as [`ProviderError::InvalidSelection`]
//!   — a router that cannot be built correctly is never silently skipped.
//! - Credential-free backends (ollama, codex, claude-cli, prime) ignore pools
//!   with a loud warning; pooled builds use the keyed constructors, so key
//!   material flows only into the provider instance that will use it.

use std::collections::HashMap;
use std::sync::Arc;

use ardur_config::RouterTable;
use ardur_provider_openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use ardur_provider_openrouter::{OpenRouterConfig, OpenRouterProvider};
use ardur_provider_runtime::{
    AnthropicProvider, ChainEntry, CredentialId, ModelOverrideSpec, Provider, ProviderError,
    RouterProvider,
};

use crate::{ModelId, ProviderKind};

/// The kill-switch environment variable. `off`/`0`/`false` (case-insensitive)
/// forces the pre-router single-provider boot path.
pub const ROUTER_ENV: &str = "ARDUR_ROUTER";

/// The environment-variable pattern for credential pools:
/// `ARDUR_<PROVIDER>_KEYS`, with the backend spelling uppercased and `-`
/// mapped to `_` (`openai-compat` → `ARDUR_OPENAI_COMPAT_KEYS`). Values are
/// comma-separated; empty entries are dropped.
pub fn pool_env_name(backend: &str) -> String {
    format!(
        "ARDUR_{}_KEYS",
        backend.to_ascii_uppercase().replace('-', "_")
    )
}

/// Whether the kill-switch is engaged.
fn router_disabled() -> bool {
    std::env::var(ROUTER_ENV)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "off" | "0" | "false"
            )
        })
        .unwrap_or(false)
}

/// The router branch of [`crate::from_env`].
///
/// Returns `Ok(None)` when boot should continue down the historical
/// single-provider path: the kill-switch is set, no config path can be
/// resolved, or the config file holds no `[router]` table. Any *real* error
/// — unreadable file, malformed table, unbuildable lane — is an `Err` that
/// aborts boot.
pub fn router_from_env() -> Result<Option<Arc<dyn Provider>>, ProviderError> {
    if router_disabled() {
        return Ok(None);
    }
    let Some(path) = ardur_config::default_config_path() else {
        return Ok(None);
    };
    let table = ardur_config::load_router_table(&path)
        .map_err(|e| ProviderError::InvalidSelection(format!("loading [router] config: {e}")))?;
    let Some(table) = table else {
        return Ok(None);
    };
    if let Ok(selector) = std::env::var(crate::SELECTOR_ENV) {
        if !selector.trim().is_empty() {
            tracing::warn!(
                selector = %selector,
                "[router] table is configured: the router owns provider selection and \
                 ARDUR_PROVIDER is superseded (set ARDUR_ROUTER=off to force the \
                 single-provider path)"
            );
        }
    }
    let router = build_router_provider(&table)?;
    tracing::info!(
        lanes = router.lane_count(),
        default_lane_entries = router.lane_len("default").unwrap_or(0),
        "[router] model router active"
    );
    Ok(Some(Arc::new(router)))
}

/// Build a concrete [`RouterProvider`] from a parsed table. Exposed so tests
/// (and future boot paths) can inspect the router's lanes directly.
///
/// # Errors
///
/// Returns [`ProviderError::InvalidSelection`] for an unknown backend
/// spelling, a missing default lane, or an empty lane chain; and whatever
/// [`ProviderError`] a credentialed backend's constructor raises for its
/// implicit (unpooled) credential — e.g. [`ProviderError::Unauthorized`] for a
/// missing `ANTHROPIC_API_KEY`, exactly as a direct selection would.
pub fn build_router_provider(table: &RouterTable) -> Result<RouterProvider, ProviderError> {
    let mut lanes: Vec<(String, Vec<ChainEntry>)> = Vec::new();
    for (class, chain) in &table.lanes {
        let mut entries = Vec::new();
        for lane_entry in chain {
            let kind = ProviderKind::parse(&lane_entry.backend)
                .map_err(|e| ProviderError::InvalidSelection(e.to_string()))?;
            let model = ModelId::new(&lane_entry.model);
            build_entry_instances(kind, &model, table, &mut entries)?;
        }
        lanes.push((class.clone(), entries));
    }

    let overrides: HashMap<String, ModelOverrideSpec> = table
        .model_overrides
        .iter()
        .map(|(model, patch)| {
            (
                model.clone(),
                ModelOverrideSpec {
                    context_window: patch.context_window,
                    input_price_micros: patch.input_price_micros,
                    output_price_micros: patch.output_price_micros,
                },
            )
        })
        .collect();

    RouterProvider::new(&table.default, lanes, overrides)
        .map_err(|e| ProviderError::InvalidSelection(e.to_string()))
}

/// Expand one configured lane entry into one chain entry per credential.
///
/// Pool resolution: `ARDUR_<PROVIDER>_KEYS` wins; `[router.credential_pools]`
/// is the fallback; with neither, the backend is built exactly as a direct
/// selection would build it (its own ambient credential, or its infallible
/// credential-free constructor).
fn build_entry_instances(
    kind: ProviderKind,
    model: &ModelId,
    table: &RouterTable,
    entries: &mut Vec<ChainEntry>,
) -> Result<(), ProviderError> {
    let backend = kind.as_str();
    let env_keys: Vec<String> = std::env::var(pool_env_name(backend))
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let keyed = match kind {
        ProviderKind::Anthropic | ProviderKind::OpenRouter | ProviderKind::OpenAiCompat => true,
        ProviderKind::Ollama
        | ProviderKind::Codex
        | ProviderKind::ClaudeCli
        | ProviderKind::Prime => false,
    };

    if !keyed {
        if !env_keys.is_empty() || table.credential_pools.contains_key(backend) {
            tracing::warn!(
                backend,
                "[router] credential pool configured for a credential-free backend; \
                 the pool is ignored"
            );
        }
        let provider = kind.build(model.clone())?;
        entries.push(chain_entry(backend, model, provider, 0, table));
        return Ok(());
    }

    // The pool for this backend, env first. `PooledKey::expose` is the single
    // point where stored key material leaves its redacted wrapper.
    let pool: Vec<String> = if !env_keys.is_empty() {
        env_keys
    } else {
        table
            .credential_pools
            .get(backend)
            .map(|keys| keys.iter().map(|k| k.expose().to_string()).collect())
            .unwrap_or_default()
    };

    if pool.is_empty() {
        // Today's behavior, unchanged: one instance on the ambient credential
        // (whose constructor raises the same error a direct selection would).
        let provider = kind.build(model.clone())?;
        entries.push(chain_entry(backend, model, provider, 0, table));
        return Ok(());
    }

    for (index, key) in pool.iter().enumerate() {
        let provider: Arc<dyn Provider> = match kind {
            ProviderKind::Anthropic => Arc::new(AnthropicProvider::new(key.clone(), model.clone())),
            ProviderKind::OpenRouter => Arc::new(OpenRouterProvider::new(
                OpenRouterConfig::new(key.clone()),
                model.clone(),
            )),
            ProviderKind::OpenAiCompat => Arc::new(OpenAiCompatProvider::new(
                OpenAiCompatConfig::new(key.clone()),
                model.clone(),
            )),
            // `keyed` was checked above; reaching another kind here is a bug in
            // this function, not an operator error.
            ProviderKind::Ollama
            | ProviderKind::Codex
            | ProviderKind::ClaudeCli
            | ProviderKind::Prime => unreachable!("credential-free kind handled above"),
        };
        entries.push(chain_entry(backend, model, provider, index, table));
    }
    Ok(())
}

/// Wrap a built provider as a chain entry with this table's override for its
/// model applied.
fn chain_entry(
    backend: &str,
    model: &ModelId,
    provider: Arc<dyn Provider>,
    index: usize,
    table: &RouterTable,
) -> ChainEntry {
    let patch = table
        .model_overrides
        .get(&model.0)
        .map(|p| ModelOverrideSpec {
            context_window: p.context_window,
            input_price_micros: p.input_price_micros,
            output_price_micros: p.output_price_micros,
        });
    ChainEntry::new(
        backend,
        model.clone(),
        provider,
        CredentialId {
            backend: backend.to_string(),
            index,
        },
        patch.as_ref(),
    )
}
