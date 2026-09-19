use crate::{Role, Update, Verdict};
use ardur_fused_runtime::StageKind;
use ardur_provider_runtime::Usage;
use ardur_runtime::RuntimeError;
use std::time::Duration;

#[derive(Default)]
pub(super) struct Status {
    pub capacity: Option<u64>,
    pub budget: Option<u64>,
    pub pending: bool,
    pub cost: Option<u64>,
    pub verdict: Verdict,
    pub latest_usage: Option<Usage>,
    stage: Option<StageKind>,
    failure: Option<&'static str>,
}
impl Status {
    pub fn begin(&mut self) {
        *self = Self {
            capacity: self.capacity,
            budget: self.budget,
            pending: true,
            ..Self::default()
        };
    }

    pub fn reduce(&mut self, update: &Update) {
        match update {
            Update::StageStart { stage } => self.stage = Some(*stage),
            Update::StageEnd { stage, .. } if self.stage == Some(*stage) => self.stage = None,
            Update::Usage { usage, .. } => self.latest_usage = Some(*usage),
            Update::ReceiptMinted {
                total_cost_cents, ..
            } => self.cost = Some(*total_cost_cents),
            Update::Error(error) => self.failure = Some(safe_error(error)),
            Update::Verdict(verdict) => self.verdict = *verdict,
            Update::StageEnd { .. }
            | Update::ContentDelta(_)
            | Update::ToolCallStart { .. }
            | Update::ToolCallDelta { .. }
            | Update::ToolCallResult { .. }
            | Update::Finish(_) => {}
        }
    }
    pub fn context(&self) -> (String, Role) {
        match (self.latest_usage, self.capacity.filter(|c| *c > 0)) {
            (Some(u), Some(cap)) => {
                let percent = u.tokens_in as u128 * 100 / cap as u128;
                let role = if percent >= 85 {
                    Role::Error
                } else if percent >= 50 {
                    Role::Warn
                } else {
                    Role::Dim
                };
                (
                    format!(
                        "ctx {percent}% · {} latest input / {cap} configured",
                        u.tokens_in
                    ),
                    role,
                )
            }
            (Some(u), None) => (
                format!("ctx unknown capacity · {} latest input", u.tokens_in),
                Role::Dim,
            ),
            (None, _) => ("ctx unknown · no usage observed".into(), Role::Dim),
        }
    }
    pub fn activity(&self, elapsed: Duration, tick: usize, animate: bool) -> String {
        if let Some(error) = self.failure {
            return error.into();
        }
        if !self.pending {
            return "Ready · /help for keys and commands".into();
        }
        let verb = match self.stage {
            Some(StageKind::CapTokenVerify) => "Verifying capability",
            Some(StageKind::CedarCheck) => "Checking policy",
            Some(StageKind::InjectionScan) => "Scanning content",
            Some(StageKind::CostGateAdmit) => "Admitting budget",
            Some(StageKind::ProviderStream) => "Receiving response",
            Some(StageKind::ToolExec) => "Running tools",
            Some(StageKind::ReceiptMint) => "Minting receipt",
            Some(StageKind::CostGateFinalize) => "Settling cost",
            Some(StageKind::MemoryRecord) => "Recording memory",
            Some(StageKind::JournalAppend) => "Appending journal",
            None => "Advancing turn",
        };
        let glyph = if animate {
            ["⠋", "⠙", "⠹", "⠸"][tick % 4]
        } else {
            "·"
        };
        let tokens = self.latest_usage.map_or_else(
            || "tokens unknown".into(),
            |u| format!("{} out (observed)", u.tokens_out),
        );
        format!("{glyph} {verb} · {:.1}s · {tokens}", elapsed.as_secs_f64())
    }
    pub fn error(&self) -> Option<&'static str> {
        self.failure
    }
    pub fn verdict_label(&self) -> (&'static str, Role) {
        match self.verdict {
            Verdict::InsufficientEvidence => {
                ("? unverified · verification evidence missing", Role::Warn)
            }
            Verdict::Compliant => ("✓ compliant · explicit verification", Role::Success),
            Verdict::Violation => ("✗ violation · explicit verification", Role::Error),
        }
    }
    pub fn budget_label(&self) -> String {
        let budget = self
            .budget
            .map_or_else(|| "unknown".into(), |n| format!("{n}¢"));
        let cost = self
            .cost
            .map_or_else(|| "unknown".into(), |n| format!("{n}¢"));
        format!(
            "turn receipt {cost} · ledger {budget}{}",
            if self.pending {
                " (last observed; pending)"
            } else {
                " (last observed)"
            }
        )
    }
}

pub(super) fn safe_error(error: &RuntimeError) -> &'static str {
    match error {
        RuntimeError::CapTokenMissing => "✗ Denied: missing capability",
        RuntimeError::CapTokenExpired => "✗ Denied: expired capability",
        RuntimeError::CapDenied { .. } => "✗ Denied: capability",
        RuntimeError::PolicyDenied { .. } => "✗ Denied: policy",
        RuntimeError::CostCeilingExceeded => "✗ Denied: budget ceiling",
        RuntimeError::ProviderUnavailable => "✗ Provider unavailable",
        RuntimeError::TurnCancelled => "Cancelled · uncommitted output is display-only",
        RuntimeError::VetoedByHook { .. } => "✗ Denied: lifecycle hook",
        RuntimeError::ProvisioningFailed { .. } => "✗ Budget provisioning failed",
        RuntimeError::InjectionBlocked { .. } => "✗ Denied: content scan",
        RuntimeError::UnknownTool { .. } => "✗ Tool unavailable",
        RuntimeError::ToolLoopExhausted { .. } => "✗ Tool iteration limit",
        RuntimeError::ToolTimeout { .. } => "✗ Tool timed out",
        RuntimeError::StreamedContentCapExceeded { .. } => "✗ Stream content limit",
        RuntimeError::ApprovalRequired { .. } => {
            "? Approval required — use ardur approvals outside the TUI"
        }
        RuntimeError::ApprovalRejected { .. } => "✗ Approval rejected",
        RuntimeError::CommandNotFound(_) => "✗ Command unavailable",
        RuntimeError::Internal(_) => "✗ Turn failed · internal error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_fused_runtime::StageKind;
    use ardur_provider_runtime::{FinishReason, Usage};
    use ardur_runtime::{ReceiptId, RuntimeError};
    use std::time::Duration;
    #[test]
    fn observed_status_keeps_receipts_usage_capacity_and_verdict_distinct() {
        let mut s = Status::default();
        assert!(s.context().0.contains("unknown"));
        s.capacity = Some(100);
        assert!(
            s.context().0.contains("unknown"),
            "capacity alone is not usage"
        );
        for (n, role) in [
            (49, Role::Dim),
            (50, Role::Warn),
            (84, Role::Warn),
            (85, Role::Error),
        ] {
            let usage = Usage {
                tokens_in: n,
                tokens_out: 3,
                cost_cents: Some(999),
            };
            s.reduce(&Update::Usage {
                usage,
                total: Usage {
                    tokens_in: 9999,
                    ..usage
                },
            });
            assert_eq!(s.context().1, role);
            assert!(
                s.context().0.contains(&format!("{n}%")),
                "latest request, not sum"
            );
            assert_eq!(s.cost, None, "usage cost is advisory");
        }
        s.reduce(&Update::ReceiptMinted {
            receipt_id: ReceiptId(uuid::Uuid::nil()),
            chain_hash: "not verified".into(),
            cost_cents: 4,
            total_cost_cents: 7,
            committed_content: "done".into(),
        });
        s.reduce(&Update::StageEnd {
            stage: StageKind::JournalAppend,
            ok: true,
        });
        s.reduce(&Update::Finish(FinishReason::Stop));
        assert_eq!(s.cost, Some(7));
        assert_eq!(s.budget, None, "never derive ledger budget from receipts");
        assert_eq!(s.verdict, Verdict::InsufficientEvidence);
        s.pending = true;
        s.reduce(&Update::StageStart {
            stage: StageKind::ToolExec,
        });
        let a = s.activity(Duration::from_millis(1250), 1, true);
        assert!(a.contains("Running tools") && a.contains("1.2s") && a.contains("3 out"));
        assert_eq!(
            s.activity(Duration::ZERO, 0, false),
            s.activity(Duration::ZERO, 5, false)
        );
        s.capacity = None;
        assert!(s.context().0.contains("unknown capacity"));
        s.reduce(&Update::Error(RuntimeError::ApprovalRequired {
            approval_id: "secret".into(),
            tool: "secret".into(),
            reason: "secret\x1b]52;".into(),
        }));
        assert_eq!(
            s.error(),
            Some("? Approval required — use ardur approvals outside the TUI")
        );
        s.reduce(&Update::Error(RuntimeError::PolicyDenied {
            reason: "secret".into(),
        }));
        assert_eq!(s.error(), Some("✗ Denied: policy"));
    }
}
