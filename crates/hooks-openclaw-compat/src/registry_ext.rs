//! Wiring OpenClaw-format hook entries into `ardur-lifecycle-hooks`.

use std::sync::{Arc, Mutex};

use ardur_lifecycle_hooks::{
    HookDecision, HookError, HookId, HookRegistry, LifecycleHook, PostReceiptCtx, PreSubmitCtx,
};
use async_trait::async_trait;

use crate::{
    CanonicalHookEventName, DefaultOpenClawMigrationTranslator, HookResponseEnvelope,
    MigrationError, OpenClawCodexEventName, OpenClawHookConfig, OpenClawMigrationTranslator,
    TranslatedHookEntry, TranslationReport,
};

/// Failure while adapting OpenClaw-format hook config into a [`HookRegistry`].
#[derive(Debug, thiserror::Error)]
pub enum OpenClawHookRegistrationError {
    /// The OpenClaw config could not be translated into Ardur hook entries.
    #[error(transparent)]
    Migration(#[from] MigrationError),
    /// A hook runner failed while a lifecycle callback was executing.
    #[error("OpenClaw hook runner failed: {0}")]
    Runner(String),
}

/// Invocation data passed to an OpenClaw compatibility runner.
#[derive(Clone, Debug)]
pub struct OpenClawHookInvocation {
    /// The source OpenClaw/codex event name.
    pub openclaw_event: OpenClawCodexEventName,
    /// The Ardur canonical event name.
    pub canonical_event: CanonicalHookEventName,
    /// Source command string from the translated OpenClaw entry.
    pub command: String,
    /// The session id as a string.
    pub session_id: String,
    /// The cap-token handle associated with this turn.
    pub cap_token: String,
    /// Source entry index in the OpenClaw config.
    pub source_entry_index: usize,
}

/// Safe runner boundary for OpenClaw compatibility hooks.
///
/// Production can provide a subprocess runner later; the default runner is a
/// no-op and tests use [`RecordingOpenClawRunner`]. Keeping execution behind this
/// trait avoids silently shelling out just because an OpenClaw config was loaded.
#[async_trait]
pub trait OpenClawHookRunner: Send + Sync {
    /// Run one translated OpenClaw hook invocation.
    async fn run(
        &self,
        invocation: OpenClawHookInvocation,
    ) -> Result<HookResponseEnvelope, OpenClawHookRegistrationError>;
}

/// No-op runner used by [`OpenClawHookRegistryExt::register_openclaw_hooks`].
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopOpenClawRunner;

#[async_trait]
impl OpenClawHookRunner for NoopOpenClawRunner {
    async fn run(
        &self,
        _invocation: OpenClawHookInvocation,
    ) -> Result<HookResponseEnvelope, OpenClawHookRegistrationError> {
        Ok(HookResponseEnvelope::NoOp)
    }
}

/// Test/helper runner that records each fired OpenClaw command in order.
#[derive(Debug, Default)]
pub struct RecordingOpenClawRunner {
    fired: Mutex<Vec<String>>,
}

impl RecordingOpenClawRunner {
    /// Create an empty recording runner.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the commands fired so far, as `<event>:<command>` strings.
    #[must_use]
    pub fn fired_commands(&self) -> Vec<String> {
        self.fired
            .lock()
            .expect("recording OpenClaw runner lock poisoned")
            .clone()
    }
}

#[async_trait]
impl OpenClawHookRunner for RecordingOpenClawRunner {
    async fn run(
        &self,
        invocation: OpenClawHookInvocation,
    ) -> Result<HookResponseEnvelope, OpenClawHookRegistrationError> {
        self.fired
            .lock()
            .expect("recording OpenClaw runner lock poisoned")
            .push(format!(
                "{}:{}",
                invocation.openclaw_event.as_str(),
                invocation.command
            ));
        if let Some(reason) = invocation.command.strip_prefix("block:") {
            Ok(HookResponseEnvelope::Block {
                reason: reason.trim().to_string(),
            })
        } else {
            Ok(HookResponseEnvelope::NoOp)
        }
    }
}

/// Extension methods that register OpenClaw-format hooks in a lifecycle registry.
pub trait OpenClawHookRegistryExt {
    /// Translate and register OpenClaw hooks with the safe no-op runner.
    fn register_openclaw_hooks(
        &mut self,
        config: &OpenClawHookConfig,
    ) -> Result<TranslationReport, OpenClawHookRegistrationError>;

    /// Translate and register OpenClaw hooks with an explicit runner.
    fn register_openclaw_hooks_with_runner(
        &mut self,
        config: &OpenClawHookConfig,
        runner: Arc<dyn OpenClawHookRunner>,
    ) -> Result<TranslationReport, OpenClawHookRegistrationError>;
}

impl OpenClawHookRegistryExt for HookRegistry {
    fn register_openclaw_hooks(
        &mut self,
        config: &OpenClawHookConfig,
    ) -> Result<TranslationReport, OpenClawHookRegistrationError> {
        self.register_openclaw_hooks_with_runner(config, Arc::new(NoopOpenClawRunner))
    }

    fn register_openclaw_hooks_with_runner(
        &mut self,
        config: &OpenClawHookConfig,
        runner: Arc<dyn OpenClawHookRunner>,
    ) -> Result<TranslationReport, OpenClawHookRegistrationError> {
        let translator = DefaultOpenClawMigrationTranslator;
        let report = translator.translate(config)?;
        for entry in report.entries.iter().cloned() {
            self.register(Arc::new(OpenClawLifecycleHook {
                entry,
                runner: runner.clone(),
            }));
        }
        Ok(report)
    }
}

struct OpenClawLifecycleHook {
    entry: TranslatedHookEntry,
    runner: Arc<dyn OpenClawHookRunner>,
}

#[async_trait]
impl LifecycleHook for OpenClawLifecycleHook {
    async fn on_pre_submit(&self, ctx: &PreSubmitCtx<'_>) -> HookDecision {
        if !matches!(
            self.entry.canonical_event,
            CanonicalHookEventName::PreToolCall | CanonicalHookEventName::PreApprovalRequest
        ) {
            return HookDecision::Continue;
        }
        match self.runner.run(self.invocation(ctx)).await {
            Ok(HookResponseEnvelope::Block { reason }) => HookDecision::Veto { reason },
            Ok(HookResponseEnvelope::NoOp | HookResponseEnvelope::Allow) => HookDecision::Continue,
            Err(err) => HookDecision::Veto {
                reason: err.to_string(),
            },
        }
    }

    async fn on_post_receipt(&self, ctx: &PostReceiptCtx<'_>) -> Result<(), HookError> {
        if !matches!(
            self.entry.canonical_event,
            CanonicalHookEventName::PostToolCall | CanonicalHookEventName::SubagentStop
        ) {
            return Ok(());
        }
        let invocation = OpenClawHookInvocation {
            openclaw_event: self.entry.openclaw_event,
            canonical_event: self.entry.canonical_event,
            command: self.entry.command.clone(),
            session_id: ctx.session_id.0.to_string(),
            // The receipt's cap-token id is now a UUID (`ardur_core_types::TokenId`);
            // the openclaw invocation carries it as its string form.
            cap_token: ctx.receipt.cap_token_id.0.to_string(),
            source_entry_index: self.entry.source_entry_index,
        };
        match self.runner.run(invocation).await {
            Ok(HookResponseEnvelope::Block { reason }) => Err(HookError::Custom(reason)),
            Ok(HookResponseEnvelope::NoOp | HookResponseEnvelope::Allow) => Ok(()),
            Err(err) => Err(HookError::Custom(err.to_string())),
        }
    }

    fn hook_id(&self) -> HookId {
        HookId::new(format!(
            "openclaw:{}:{}",
            self.entry.openclaw_event.as_str(),
            self.entry.source_entry_index
        ))
    }

    fn priority(&self) -> i32 {
        i32::try_from(self.entry.source_entry_index).unwrap_or(i32::MAX)
    }
}

impl OpenClawLifecycleHook {
    fn invocation(&self, ctx: &PreSubmitCtx<'_>) -> OpenClawHookInvocation {
        OpenClawHookInvocation {
            openclaw_event: self.entry.openclaw_event,
            canonical_event: self.entry.canonical_event,
            command: self.entry.command.clone(),
            session_id: ctx.session_id.0.to_string(),
            cap_token: ctx.cap_token_id.0.clone(),
            source_entry_index: self.entry.source_entry_index,
        }
    }
}
