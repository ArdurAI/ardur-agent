//! Enforcement seam: derive an Ardur kernel-enforcement policy from a verified
//! grant, and hand it to the enforcer.
//!
//! Ardur's `ardur-kernelcaptured` daemon runs a per-cgroup BPF-LSM that can
//! **truly block** `exec` / file-open / IP-connect syscalls with `-EPERM` when
//! `action=DENY, enforce_mode=ENFORCE`. Policy reaches it as a
//! `DaemonApplyPolicyRequest` and a workload is attached by cgroup binding.
//!
//! This module derives that policy from the effective capability set the
//! cap-token verifier enforced — so the userland tool-call gate
//! (`authorize_tool_capabilities`) and the kernel syscall gate enforce the
//! *same* authority (defense in depth). It emits the request in the daemon's
//! shape via [`EnforcementProfile::to_daemon_request_json`].
//!
//! **Platform boundary:** true LSM deny is Linux + managed-cgroup only; the
//! agent's dev host is macOS, which has no BPF-LSM. The [`EnforcementAttach`]
//! trait is the handoff seam; [`RecordingAttach`] captures the derived profile
//! for audit/tests without a kernel, and the real Linux attach (over the
//! daemon's socket contract, cross-repo dependency CR-3) is left as the
//! deployment-side implementation.

use ardur_cap_token::VerifiedClaims;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::GovernanceError;

/// A guarded kernel operation (mirrors Ardur `BpfOp`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnforceOp {
    /// `execve`/`execveat` (`bprm_check_security`). Numeric `0x01`.
    Exec,
    /// File open for read (`file_open`, `O_RDONLY`). Numeric `0x02`.
    FileRead,
    /// File open for write (`file_open`, write flags). Numeric `0x03`.
    FileWrite,
    /// Outbound IPv4/IPv6 connect (`socket_connect`). Numeric `0x04`.
    NetConnect,
    /// External send. Numeric `0x05`.
    ExternalSend,
}

impl EnforceOp {
    /// The Ardur `BpfOp` numeric code.
    pub fn code(self) -> u8 {
        match self {
            EnforceOp::Exec => 0x01,
            EnforceOp::FileRead => 0x02,
            EnforceOp::FileWrite => 0x03,
            EnforceOp::NetConnect => 0x04,
            EnforceOp::ExternalSend => 0x05,
        }
    }
}

/// The action for a guarded op (mirrors Ardur `BpfAction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnforceAction {
    /// Always permit. Numeric `0`.
    Allow,
    /// Deny (`-EPERM` in ENFORCE mode; logged in PERMISSIVE). Numeric `1`.
    Deny,
    /// Permit only against the path/net allowlist. Numeric `2`.
    Allowlist,
}

impl EnforceAction {
    /// The Ardur `BpfAction` numeric code.
    pub fn code(self) -> u8 {
        match self {
            EnforceAction::Allow => 0,
            EnforceAction::Deny => 1,
            EnforceAction::Allowlist => 2,
        }
    }
}

/// Enforcement strength (mirrors Ardur `BpfEnforceMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnforceMode {
    /// Observe/log only. Numeric `0`.
    Permissive,
    /// True kernel block. Numeric `1`.
    Enforce,
}

impl EnforceMode {
    /// The Ardur `BpfEnforceMode` numeric code.
    pub fn code(self) -> u8 {
        match self {
            EnforceMode::Permissive => 0,
            EnforceMode::Enforce => 1,
        }
    }
}

/// One per-op policy entry (mirrors Ardur `DaemonOpPolicy`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpPolicy {
    /// The guarded op.
    pub op: EnforceOp,
    /// The action for this op.
    pub action: EnforceAction,
    /// Per-op enforcement mode.
    pub enforce_mode: EnforceMode,
}

/// A kernel-enforcement policy derived from a verified grant, shaped to the
/// Ardur daemon's `DaemonApplyPolicyRequest`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnforcementProfile {
    /// The governed session id (the cgroup-bound session key).
    pub session_id: String,
    /// Per-op decisions.
    pub op_policies: Vec<OpPolicy>,
    /// Allowlisted filesystem path prefixes (for `FileRead`/`FileWrite`
    /// `Allowlist`).
    pub path_allow: Vec<String>,
    /// Allowlisted network CIDRs (for `NetConnect` `Allowlist`).
    pub net_allow: Vec<String>,
    /// Default enforcement mode for ops with no explicit rule.
    pub enforce_mode: EnforceMode,
}

impl EnforcementProfile {
    /// Derive a profile from the effective capability set the cap-token verifier
    /// enforced. Capability strings are the runtime's `Capability::as_str()`
    /// form (`cap.shell_exec`, `cap.fs_read`, ...). A capability present ⇒ its
    /// op is permitted (allowlisted to `cwd`/`net_allow` where path/net scoped);
    /// absent ⇒ its op is denied — the same authority the userland gate applied.
    pub fn from_claims(
        session_id: impl Into<String>,
        claims: &VerifiedClaims,
        cwd: &str,
        net_allow: Vec<String>,
        mode: EnforceMode,
    ) -> Self {
        let has = |cap: &str| claims.tool_allowlist.iter().any(|t| t == cap);
        let exec = has("cap.shell_exec") || has("cap.process_spawn");
        let read = has("cap.fs_read");
        let write = has("cap.fs_write");
        let net = has("cap.network_out");

        let mut op_policies = Vec::new();
        let gate = |allowed: bool, allowlisted: bool| -> EnforceAction {
            if !allowed {
                EnforceAction::Deny
            } else if allowlisted {
                EnforceAction::Allowlist
            } else {
                EnforceAction::Allow
            }
        };
        op_policies.push(OpPolicy {
            op: EnforceOp::Exec,
            action: gate(exec, false),
            enforce_mode: mode,
        });
        op_policies.push(OpPolicy {
            op: EnforceOp::FileRead,
            action: gate(read, true),
            enforce_mode: mode,
        });
        op_policies.push(OpPolicy {
            op: EnforceOp::FileWrite,
            action: gate(write, true),
            enforce_mode: mode,
        });
        op_policies.push(OpPolicy {
            op: EnforceOp::NetConnect,
            action: gate(net, !net_allow.is_empty()),
            enforce_mode: mode,
        });

        let path_allow = if read || write {
            vec![cwd.to_string()]
        } else {
            Vec::new()
        };

        Self {
            session_id: session_id.into(),
            op_policies,
            path_allow,
            net_allow,
            enforce_mode: mode,
        }
    }

    /// Project to the Ardur daemon's `DaemonApplyPolicyRequest` JSON shape. The
    /// exact field tags are pending the published IPC contract (CR-3); this is
    /// the proposed encoding, with numeric `BpfOp`/`BpfAction`/`BpfEnforceMode`
    /// codes.
    pub fn to_daemon_request_json(&self) -> serde_json::Value {
        json!({
            "session_id": self.session_id,
            "op_policies": self.op_policies.iter().map(|p| json!({
                "op": p.op.code(),
                "action": p.action.code(),
                "enforce_mode": p.enforce_mode.code(),
            })).collect::<Vec<_>>(),
            "path_allow": self.path_allow,
            "net_allow": self.net_allow,
            "enforce_mode": self.enforce_mode.code(),
        })
    }
}

/// The handoff seam: apply a derived [`EnforcementProfile`] to the enforcer.
pub trait EnforcementAttach {
    /// Apply `profile`, binding it to the governed workload's cgroup.
    fn apply(&self, profile: &EnforcementProfile) -> Result<(), GovernanceError>;
}

/// An [`EnforcementAttach`] that records the last-applied profile instead of
/// touching a kernel — the portable default (and test double) for hosts without
/// BPF-LSM. It never claims a kernel block it did not perform.
#[derive(Debug, Default)]
pub struct RecordingAttach {
    applied: std::sync::Mutex<Vec<EnforcementProfile>>,
}

impl RecordingAttach {
    /// A fresh recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every profile applied so far, in order.
    pub fn applied(&self) -> Vec<EnforcementProfile> {
        self.applied.lock().expect("recorder mutex").clone()
    }
}

impl EnforcementAttach for RecordingAttach {
    fn apply(&self, profile: &EnforcementProfile) -> Result<(), GovernanceError> {
        self.applied
            .lock()
            .expect("recorder mutex")
            .push(profile.clone());
        Ok(())
    }
}
