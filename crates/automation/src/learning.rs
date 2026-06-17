//! Closed learning loop: mine past sessions into human-approved playbook proposals.
//!
//! The loop is intentionally closed and approval-gated: a scheduled job may mine
//! patterns and write proposals, but proposals remain `PendingHumanApproval`
//! until an explicit human approval transitions them. The job is authorized by an
//! already-verified cap-token plus a Cedar policy check, and every proposal is
//! linked into a receipt-style hash chain for auditability.

use std::collections::BTreeMap;

use ardur_cap_token::VerifiedClaims;
use ardur_cedar_policy::{
    ActionRef, CedarPolicyBundle, Decision, EvaluationContext, PolicyBundle, PrincipalRef,
    ResourceRef,
};
use ardur_receipt::Sha256Digest;
use serde::{Deserialize, Serialize};

/// Capability required for the background learning/dreaming job.
pub const LEARNING_DREAM_TOOL: &str = "learning.dream";
/// Cedar action used by the policy debugger/learning loop for dreaming runs.
pub const LEARNING_DREAM_ACTION: &str = "Action::\"LearningDream\"";

/// One past session reduced to the text the learning job mines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningSession {
    /// Stable session id.
    pub session_id: String,
    /// Workspace/lane the session belongs to.
    pub workspace_id: String,
    /// User/assistant messages, newest or oldest order accepted.
    pub messages: Vec<String>,
}

/// Human approval state for a mined playbook proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalState {
    /// Mined by the learning job but not yet usable by future sessions.
    PendingHumanApproval,
    /// Approved by a named human.
    Approved,
}

/// A structured playbook candidate mined from repeated session patterns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookProposal {
    /// Stable proposal id.
    pub proposal_id: uuid::Uuid,
    /// Human title.
    pub title: String,
    /// Normalized pattern text.
    pub pattern: String,
    /// Number of duplicate/supporting examples merged into this proposal.
    pub occurrences: usize,
    /// Source sessions supporting the pattern.
    pub source_sessions: Vec<String>,
    /// Approval gate state.
    pub status: ApprovalState,
    /// Receipt id for the proposal write.
    pub receipt_id: uuid::Uuid,
    /// Hash of the prior proposal receipt in this run, if any.
    pub parent_hash: Option<Sha256Digest>,
    /// Human approver, once approved.
    pub approved_by: Option<String>,
}

impl PlaybookProposal {
    /// Canonical bytes hashed by the next proposal's `parent_hash`.
    #[must_use]
    pub fn chain_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

/// A completed dreaming-job run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamingReport {
    /// Number of input sessions considered after workspace filtering.
    pub sessions_read: usize,
    /// Number of duplicate messages merged.
    pub duplicates_merged: usize,
    /// New playbook proposals.
    pub proposals: Vec<PlaybookProposal>,
}

/// Schedule configuration for one background dreaming job tick.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamingJobConfig {
    /// Workspace/lane to mine.
    pub workspace_id: String,
    /// Number of most-recent sessions to read for this tick.
    pub past_session_limit: usize,
}

/// Learning-loop failures.
#[derive(Debug, thiserror::Error)]
pub enum LearningError {
    /// Cap-token claims did not grant the learning capability.
    #[error("cap-token does not grant {LEARNING_DREAM_TOOL}")]
    CapabilityDenied,
    /// Cedar denied the learning action.
    #[error("cedar denied learning job: {0}")]
    PolicyDenied(String),
    /// Approval attempted without a human decision.
    #[error("playbook proposals require explicit human approval")]
    ApprovalRequired,
}

/// Closed learning-loop engine.
pub struct LearningLoop {
    policies: CedarPolicyBundle,
    chain_tail: Option<Sha256Digest>,
}

impl LearningLoop {
    /// Build a learning loop with a Cedar policy bundle.
    #[must_use]
    pub fn new(policies: CedarPolicyBundle) -> Self {
        Self {
            policies,
            chain_tail: None,
        }
    }

    /// Run the scheduled dreaming job over the last N sessions for `workspace_id`.
    ///
    /// The caller supplies already-verified cap-token claims. This method still
    /// enforces capability membership and Cedar policy before mining any content.
    pub fn run_dreaming_job(
        &mut self,
        claims: &VerifiedClaims,
        workspace_id: &str,
        sessions: &[LearningSession],
    ) -> Result<DreamingReport, LearningError> {
        enforce_learning_authority(&self.policies, claims, workspace_id)?;

        let mut by_pattern: BTreeMap<String, Vec<&LearningSession>> = BTreeMap::new();
        for session in sessions
            .iter()
            .filter(|session| session.workspace_id == workspace_id)
        {
            for message in &session.messages {
                let normalized = normalize(message);
                if normalized.len() >= 12 {
                    by_pattern.entry(normalized).or_default().push(session);
                }
            }
        }

        let sessions_read = sessions
            .iter()
            .filter(|session| session.workspace_id == workspace_id)
            .count();
        let duplicates_merged = by_pattern
            .values()
            .filter(|support| support.len() > 1)
            .map(|support| support.len().saturating_sub(1))
            .sum();

        let mut proposals = Vec::new();
        for (pattern, support) in by_pattern
            .into_iter()
            .filter(|(_, support)| support.len() > 1)
        {
            let source_sessions = support
                .iter()
                .map(|session| session.session_id.clone())
                .collect::<Vec<_>>();
            let proposal = self.proposal(pattern, source_sessions);
            self.chain_tail = Some(Sha256Digest::of(&proposal.chain_bytes()));
            proposals.push(proposal);
        }

        Ok(DreamingReport {
            sessions_read,
            duplicates_merged,
            proposals,
        })
    }

    /// Run one scheduled dreaming-job tick over the last `N` sessions.
    ///
    /// The scheduler/daemon chooses when this method is called; this method owns
    /// the job semantics for a tick: cap-token + Cedar enforcement, newest-window
    /// slicing, duplicate merging, playbook proposal writing, and receipt-style
    /// proposal chaining. `sessions` are expected in chronological order, with
    /// the newest session at the end.
    pub fn run_scheduled_dreaming_job(
        &mut self,
        claims: &VerifiedClaims,
        config: DreamingJobConfig,
        sessions: &[LearningSession],
    ) -> Result<DreamingReport, LearningError> {
        let start = sessions.len().saturating_sub(config.past_session_limit);
        self.run_dreaming_job(claims, &config.workspace_id, &sessions[start..])
    }

    fn proposal(&self, pattern: String, source_sessions: Vec<String>) -> PlaybookProposal {
        PlaybookProposal {
            proposal_id: uuid::Uuid::new_v4(),
            title: title_for(&pattern),
            occurrences: source_sessions.len(),
            source_sessions,
            pattern,
            status: ApprovalState::PendingHumanApproval,
            receipt_id: uuid::Uuid::new_v4(),
            parent_hash: self.chain_tail,
            approved_by: None,
        }
    }

    /// Approve a proposal. `human_approved=false` is rejected to keep the loop
    /// closed: the model/job cannot self-approve its own playbooks.
    pub fn approve(
        &self,
        mut proposal: PlaybookProposal,
        approved_by: &str,
        human_approved: bool,
    ) -> Result<PlaybookProposal, LearningError> {
        if !human_approved {
            return Err(LearningError::ApprovalRequired);
        }
        proposal.status = ApprovalState::Approved;
        proposal.approved_by = Some(approved_by.to_string());
        Ok(proposal)
    }
}

fn enforce_learning_authority(
    policies: &CedarPolicyBundle,
    claims: &VerifiedClaims,
    workspace_id: &str,
) -> Result<(), LearningError> {
    if !claims
        .tool_allowlist
        .iter()
        .any(|tool| tool == LEARNING_DREAM_TOOL)
    {
        return Err(LearningError::CapabilityDenied);
    }

    let decision = policies.evaluate(&EvaluationContext {
        principal: PrincipalRef(format!("User::\"{}\"", claims.subject.0)),
        action: ActionRef(LEARNING_DREAM_ACTION.to_string()),
        resource: ResourceRef(format!("Workspace::\"{workspace_id}\"")),
        attributes: serde_json::json!({
            "subject": claims.subject.0,
            "audience": claims.audience,
            "tool": LEARNING_DREAM_TOOL,
            "workspace_id": workspace_id,
        }),
    });

    match decision {
        Decision::Allow { .. } => Ok(()),
        Decision::Deny { reason, .. } | Decision::Indeterminate { reason } => {
            Err(LearningError::PolicyDenied(reason))
        }
    }
}

fn normalize(message: &str) -> String {
    message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_ascii_lowercase()
}

fn title_for(pattern: &str) -> String {
    let mut title = pattern.chars().take(64).collect::<String>();
    if pattern.chars().count() > 64 {
        title.push('…');
    }
    format!("Playbook: {title}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_cap_token::HolderId;
    use ardur_cedar_policy::{PolicyBundle, PolicySource};

    fn claims(tools: &[&str]) -> VerifiedClaims {
        VerifiedClaims {
            token_id: uuid::Uuid::new_v4(),
            audience: "ardur".to_string(),
            subject: HolderId("user:learning".to_string()),
            expires_unix: 2_000_000_000,
            budget_remaining: 100,
            tool_allowlist: tools.iter().map(|tool| (*tool).to_string()).collect(),
        }
    }

    fn allow_policy() -> CedarPolicyBundle {
        CedarPolicyBundle::load(PolicySource::Embedded(
            "permit(principal, action, resource);".to_string(),
        ))
        .expect("policy")
    }

    #[test]
    fn dreaming_job_merges_duplicates_and_requires_human_approval() {
        let mut loop_ = LearningLoop::new(allow_policy());
        let sessions = vec![
            LearningSession {
                session_id: "s1".to_string(),
                workspace_id: "w".to_string(),
                messages: vec!["Always run cargo test before handoff".to_string()],
            },
            LearningSession {
                session_id: "s2".to_string(),
                workspace_id: "w".to_string(),
                messages: vec![" always   run cargo test before handoff ".to_string()],
            },
            LearningSession {
                session_id: "s3".to_string(),
                workspace_id: "other".to_string(),
                messages: vec!["Always run cargo test before handoff".to_string()],
            },
        ];

        let report = loop_
            .run_dreaming_job(&claims(&[LEARNING_DREAM_TOOL]), "w", &sessions)
            .expect("dreaming job runs");

        assert_eq!(report.sessions_read, 2);
        assert_eq!(report.duplicates_merged, 1);
        assert_eq!(report.proposals.len(), 1);
        let proposal = &report.proposals[0];
        assert_eq!(proposal.occurrences, 2);
        assert_eq!(proposal.status, ApprovalState::PendingHumanApproval);
        assert!(proposal.parent_hash.is_none());

        let denied = loop_.approve(proposal.clone(), "rahul", false);
        assert!(matches!(denied, Err(LearningError::ApprovalRequired)));
        let approved = loop_.approve(proposal.clone(), "rahul", true).unwrap();
        assert_eq!(approved.status, ApprovalState::Approved);
        assert_eq!(approved.approved_by.as_deref(), Some("rahul"));
    }

    #[test]
    fn dreaming_job_enforces_capability_and_cedar() {
        let mut loop_ = LearningLoop::new(allow_policy());
        let err = loop_
            .run_dreaming_job(&claims(&["chat.submit"]), "w", &[])
            .expect_err("missing capability denies");
        assert!(matches!(err, LearningError::CapabilityDenied));

        let deny = CedarPolicyBundle::load(PolicySource::Embedded(
            "forbid(principal, action, resource);".to_string(),
        ))
        .unwrap();
        let mut loop_ = LearningLoop::new(deny);
        let err = loop_
            .run_dreaming_job(&claims(&[LEARNING_DREAM_TOOL]), "w", &[])
            .expect_err("cedar denies");
        assert!(matches!(err, LearningError::PolicyDenied(_)));
    }

    #[test]
    fn proposals_are_hash_chained() {
        let mut loop_ = LearningLoop::new(allow_policy());
        let sessions = vec![
            LearningSession {
                session_id: "s1".to_string(),
                workspace_id: "w".to_string(),
                messages: vec![
                    "pattern one repeated".to_string(),
                    "pattern two repeated".to_string(),
                ],
            },
            LearningSession {
                session_id: "s2".to_string(),
                workspace_id: "w".to_string(),
                messages: vec![
                    "pattern one repeated".to_string(),
                    "pattern two repeated".to_string(),
                ],
            },
        ];

        let report = loop_
            .run_dreaming_job(&claims(&[LEARNING_DREAM_TOOL]), "w", &sessions)
            .unwrap();
        assert_eq!(report.proposals.len(), 2);
        assert_eq!(report.proposals[0].parent_hash, None);
        assert_eq!(
            report.proposals[1].parent_hash,
            Some(Sha256Digest::of(&report.proposals[0].chain_bytes()))
        );
    }

    #[test]
    fn scheduled_dreaming_job_reads_only_last_n_sessions() {
        let mut loop_ = LearningLoop::new(allow_policy());
        let sessions = vec![
            LearningSession {
                session_id: "old-1".to_string(),
                workspace_id: "w".to_string(),
                messages: vec!["old duplicate should be ignored".to_string()],
            },
            LearningSession {
                session_id: "recent-1".to_string(),
                workspace_id: "w".to_string(),
                messages: vec!["recent duplicate becomes playbook".to_string()],
            },
            LearningSession {
                session_id: "recent-2".to_string(),
                workspace_id: "w".to_string(),
                messages: vec!["recent duplicate becomes playbook".to_string()],
            },
        ];

        let report = loop_
            .run_scheduled_dreaming_job(
                &claims(&[LEARNING_DREAM_TOOL]),
                DreamingJobConfig {
                    workspace_id: "w".to_string(),
                    past_session_limit: 2,
                },
                &sessions,
            )
            .expect("scheduled dreaming job runs");

        assert_eq!(report.sessions_read, 2);
        assert_eq!(report.duplicates_merged, 1);
        assert_eq!(report.proposals.len(), 1);
        assert_eq!(
            report.proposals[0].source_sessions,
            vec!["recent-1".to_string(), "recent-2".to_string()]
        );
    }
}
