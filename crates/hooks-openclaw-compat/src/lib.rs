//! ardur-hooks-openclaw-compat — codex-format OpenClaw hook compatibility.
//!
//! Plan family: §9.6 (`plans/9.6-openclaw-hooks-blueprint.md`).
//!
//! This crate is the pure compatibility boundary for OpenClaw migrators. It
//! does not run subprocesses, expose the OpenClaw relay daemon, or wire hooks
//! into the runtime. Instead, it owns the deterministic pieces later runtime
//! and CLI code depend on:
//!
//! - [`OpenClawHookEventNameMap`] maps OpenClaw codex event names to Ardur's
//!   canonical hook event names.
//! - [`DefaultOpenClawPayloadSerializer`] emits the codex
//!   `NativeHookRelayInvocation` JSON shape.
//! - [`DefaultOpenClawResponseParser`] recognizes the codex stdout response
//!   shapes and applies Ardur's permission-veto downgrade invariant.
//! - [`DefaultOpenClawMigrationTranslator`] turns parsed OpenClaw hook entries
//!   into Ardur-format translation records with `format: "openclaw"`.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod adapter_kind;
mod event_name_map;
mod migration_translator;
mod openclaw_source;
mod payload_serializer;
mod registry_ext;
mod response_parser;

mod sealed {
    pub trait Sealed {}
}

pub use adapter_kind::AdapterKind;
pub use event_name_map::{
    CanonicalHookEventName, OpenClawCodexEventName, OpenClawHookEventNameMap,
    OpenClawNativeEventName,
};
pub use migration_translator::{
    DefaultOpenClawMigrationTranslator, MigrationError, MigrationWarning, MigrationWarningReason,
    OpenClawConfigFormat, OpenClawHookConfig, OpenClawHookEntry, OpenClawMigrationTranslator,
    TranslatedHookEntry, TranslationReport,
};
pub use openclaw_source::OpenClawHookSource;
pub use payload_serializer::{
    CanonicalHookFirePayload, CodexStdinPayload, DefaultOpenClawPayloadSerializer,
    OpenClawHookMeta, OpenClawHookProvider, OpenClawPayloadSerializer, SerializeError,
};
pub use registry_ext::{
    NoopOpenClawRunner, OpenClawHookInvocation, OpenClawHookRegistrationError,
    OpenClawHookRegistryExt, OpenClawHookRunner, RecordingOpenClawRunner,
};
pub use response_parser::{
    CodexResponseShape, DefaultOpenClawResponseParser, HookResponseEnvelope,
    OpenClawPermissionDecision, OpenClawResponseParser, OpenClawResponseWarning, ParseError,
    ParsedOpenClawResponse, PermissionBehavior,
};
