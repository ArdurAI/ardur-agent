use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::adapter_kind::AdapterKind;
use crate::event_name_map::{
    CanonicalHookEventName, OpenClawCodexEventName, OpenClawHookEventNameMap,
    OpenClawNativeEventName,
};

/// Parsed OpenClaw hook config input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenClawHookConfig {
    /// Hook entries found in the OpenClaw config.
    pub hooks: Vec<OpenClawHookEntry>,
    /// Original config path, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<PathBuf>,
    /// Original config format, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_format: Option<OpenClawConfigFormat>,
}

/// One OpenClaw hook entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenClawHookEntry {
    /// OpenClaw/codex event.
    pub event: OpenClawCodexEventName,
    /// Command to execute.
    pub command: String,
    /// Optional matcher copied into the translated entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    /// Allowed events from OpenClaw relay registration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_events: Vec<OpenClawCodexEventName>,
    /// Optional timeout in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
    /// Provider string from OpenClaw config. Any value other than `codex` is
    /// retained in warnings and scored as unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// OpenClaw config file format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenClawConfigFormat {
    /// JSON config.
    Json,
    /// TOML config.
    Toml,
    /// YAML config.
    Yaml,
}

/// Translation report consumed by future migrate command wiring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslationReport {
    /// Translated hook entries.
    pub entries: Vec<TranslatedHookEntry>,
    /// Non-fatal migration warnings.
    pub warnings: Vec<MigrationWarning>,
    /// Source path copied from the input config.
    pub source_path: Option<PathBuf>,
    /// Translation timestamp.
    pub translated_at: SystemTime,
}

/// Ardur-format hook entry produced by translation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslatedHookEntry {
    /// Source entry index in the OpenClaw config.
    pub source_entry_index: usize,
    /// Ardur canonical event.
    pub canonical_event: CanonicalHookEventName,
    /// OpenClaw/codex source event.
    pub openclaw_event: OpenClawCodexEventName,
    /// PascalCase native event name for codex payloads.
    pub native_event_name: OpenClawNativeEventName,
    /// Command to execute.
    pub command: String,
    /// Optional matcher copied from the source entry.
    pub matcher: Option<String>,
    /// Events retained after compatibility narrowing.
    pub allowed_events: Vec<OpenClawCodexEventName>,
    /// Optional timeout in milliseconds.
    pub timeout_ms: Option<u32>,
    /// Hook adapter kind.
    pub adapter_kind: AdapterKind,
    /// Config format marker.
    pub format: &'static str,
    /// Preservation score from 0 to 100.
    pub migration_completeness: u8,
}

/// Non-fatal migration warning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationWarning {
    /// Source entry index in the OpenClaw config.
    pub source_entry_index: usize,
    /// Warning reason.
    pub reason: MigrationWarningReason,
}

/// Migration warning reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationWarningReason {
    /// OpenClaw permission veto semantics are downgraded in Ardur.
    PermissionVetoDowngraded {
        /// Source event that triggered the warning.
        source_event: OpenClawCodexEventName,
    },
    /// Provider is not the supported codex provider.
    UnsupportedProvider {
        /// Provider value found in the source config.
        provider: String,
    },
}

/// Migration failure surface.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// Hook command is missing.
    #[error("entry {source_entry_index} missing command")]
    MissingCommand {
        /// Source entry index in the OpenClaw config.
        source_entry_index: usize,
    },
}

/// Closed trait for OpenClaw migration translators.
pub trait OpenClawMigrationTranslator: crate::sealed::Sealed + Send + Sync + 'static {
    /// Translate parsed OpenClaw hook config into Ardur hook-entry records.
    fn translate(&self, config: &OpenClawHookConfig) -> Result<TranslationReport, MigrationError>;
}

/// Default pure translator used by future `ardur migrate --from-openclaw`.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultOpenClawMigrationTranslator;

impl crate::sealed::Sealed for DefaultOpenClawMigrationTranslator {}

impl OpenClawMigrationTranslator for DefaultOpenClawMigrationTranslator {
    fn translate(&self, config: &OpenClawHookConfig) -> Result<TranslationReport, MigrationError> {
        let mut entries = Vec::with_capacity(config.hooks.len());
        let mut warnings = Vec::new();

        for (index, entry) in config.hooks.iter().enumerate() {
            if entry.command.trim().is_empty() {
                return Err(MigrationError::MissingCommand {
                    source_entry_index: index,
                });
            }

            let unsupported_provider = entry
                .provider
                .as_deref()
                .filter(|provider| *provider != "codex");
            if let Some(provider) = unsupported_provider {
                warnings.push(MigrationWarning {
                    source_entry_index: index,
                    reason: MigrationWarningReason::UnsupportedProvider {
                        provider: provider.to_string(),
                    },
                });
            }

            let mut completeness = if unsupported_provider.is_some() {
                0
            } else {
                100
            };
            if entry.event == OpenClawCodexEventName::PermissionRequest {
                warnings.push(MigrationWarning {
                    source_entry_index: index,
                    reason: MigrationWarningReason::PermissionVetoDowngraded {
                        source_event: entry.event,
                    },
                });
                if completeness > 80 {
                    completeness = 80;
                }
            }

            entries.push(TranslatedHookEntry {
                source_entry_index: index,
                canonical_event: OpenClawHookEventNameMap::to_canonical(entry.event),
                openclaw_event: entry.event,
                native_event_name: entry.event.native_name(),
                command: entry.command.clone(),
                matcher: entry.matcher.clone(),
                allowed_events: entry.allowed_events.clone(),
                timeout_ms: entry.timeout_ms,
                adapter_kind: AdapterKind::OpenClaw,
                format: AdapterKind::OpenClaw.as_str(),
                migration_completeness: completeness,
            });
        }

        Ok(TranslationReport {
            entries,
            warnings,
            source_path: config.source_path.clone(),
            translated_at: SystemTime::now(),
        })
    }
}
