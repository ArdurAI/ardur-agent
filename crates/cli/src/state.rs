//! Persistent on-disk state for a FusedRuntime-backed `ardur chat` session.
//!
//! Everything the fused substrate needs to outlive a single process lands under
//! `~/.ardur/`:
//!
//! ```text
//! ~/.ardur/
//! ├── cedar.policies                     # Cedar bundle (optional; absent => deny-all unless dev fallback is explicit)
//! ├── keys/
//! │   ├── issuer.key                      # Ed25519 cap-token issuer private key (32 raw bytes)
//! │   └── receipt.pem                     # P-256 receipt signing key (PKCS#8 PEM)
//! ├── memory/                             # reserved for a file-backed memory store
//! ├── journals/sessions/<uuid>/journal.jsonl  # durable session journals
//! └── receipts/chain.jsonl               # append-only signed-receipt chain
//! ```
//!
//! The two key files are minted on first run and reused thereafter, so a
//! session's cap-token root and receipt chain are stable across restarts (the
//! receipt-chain replay the §2.E4 scenario exercises depends on this).

use std::path::{Path, PathBuf};

use ardur_cap_token::BiscuitCapTokenIssuer;
use ardur_cedar_policy::{CedarPolicyBundle, PolicyBundle, PolicySource};
use ardur_receipt::Es256SigningKey;
use biscuit_auth::{Algorithm, KeyPair, PrivateKey};

use crate::error::CliError;

/// The deny-all Cedar bundle used when no `cedar.policies` file is present.
/// Operators must either provide a real policy file or opt into the explicit
/// development escape hatch (`ARDUR_DEV_PERMISSIVE_POLICY=true`).
const DENY_ALL_POLICY: &str = "forbid(principal, action, resource);";

/// Explicit local-development fallback for ad-hoc CLI smoke tests.
const PERMISSIVE_POLICY: &str = "permit(principal, action, resource);";

/// The resolved `~/.ardur/` state directories for a session. Construct with
/// [`resolve`](StateDirs::resolve), then [`create`](StateDirs::create) to
/// materialize the tree on first run.
#[derive(Clone, Debug)]
pub struct StateDirs {
    /// `~/.ardur`.
    pub root: PathBuf,
    /// `~/.ardur/memory` — reserved for a file-backed memory store.
    pub memory: PathBuf,
    /// `~/.ardur/journals` — the base dir session journals nest under.
    pub journals: PathBuf,
    /// `~/.ardur/receipts` — holds the append-only receipt chain.
    pub receipts: PathBuf,
    /// `~/.ardur/keys` — the cap-token issuer + receipt signing keys.
    pub keys: PathBuf,
}

impl StateDirs {
    /// Resolve the `~/.ardur/` layout from the home directory. Returns
    /// [`CliError::State`] if neither `HOME` nor `USERPROFILE` is set.
    pub fn resolve() -> Result<Self, CliError> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .ok_or_else(|| {
                CliError::State("cannot resolve a home directory (HOME/USERPROFILE unset)".into())
            })?;
        let root = home.join(".ardur");
        Ok(Self {
            memory: root.join("memory"),
            journals: root.join("journals"),
            receipts: root.join("receipts"),
            keys: root.join("keys"),
            root,
        })
    }

    /// Create the state-dir tree (idempotent). Creates `memory/`, `journals/`,
    /// `receipts/`, and `keys/` under `~/.ardur/`.
    pub fn create(&self) -> Result<(), CliError> {
        for dir in [&self.memory, &self.journals, &self.receipts, &self.keys] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    /// The append-only signed-receipt chain log.
    #[must_use]
    pub fn receipt_log(&self) -> PathBuf {
        self.receipts.join("chain.jsonl")
    }

    /// The Ed25519 cap-token issuer private-key file.
    #[must_use]
    pub fn issuer_key_path(&self) -> PathBuf {
        self.keys.join("issuer.key")
    }

    /// The P-256 receipt signing-key file (PKCS#8 PEM).
    #[must_use]
    pub fn receipt_key_path(&self) -> PathBuf {
        self.keys.join("receipt.pem")
    }

    /// The optional Cedar policy file (`~/.ardur/cedar.policies`).
    #[must_use]
    pub fn cedar_path(&self) -> PathBuf {
        self.root.join("cedar.policies")
    }

    /// Load the cap-token issuer from [`issuer_key_path`](Self::issuer_key_path),
    /// minting and persisting a fresh Ed25519 root key on first run.
    pub fn load_or_create_issuer(&self) -> Result<BiscuitCapTokenIssuer, CliError> {
        let path = self.issuer_key_path();
        match std::fs::read(&path) {
            Ok(bytes) => {
                let private = PrivateKey::from_bytes(&bytes, Algorithm::Ed25519).map_err(|e| {
                    CliError::State(format!(
                        "issuer key at {} is malformed: {e}",
                        path.display()
                    ))
                })?;
                Ok(BiscuitCapTokenIssuer::new(KeyPair::from(&private)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let keypair = KeyPair::new();
                write_private(&path, &keypair.private().to_bytes())?;
                Ok(BiscuitCapTokenIssuer::new(keypair))
            }
            Err(e) => Err(CliError::Io(e)),
        }
    }

    /// Load the receipt signing key from
    /// [`receipt_key_path`](Self::receipt_key_path), generating and persisting a
    /// fresh P-256 key (PKCS#8 PEM) on first run.
    pub fn load_or_create_receipt_key(&self) -> Result<Es256SigningKey, CliError> {
        let path = self.receipt_key_path();
        match std::fs::read_to_string(&path) {
            Ok(pem) => Es256SigningKey::from_pkcs8_pem(&pem).map_err(|e| {
                CliError::State(format!(
                    "receipt key at {} is malformed: {e}",
                    path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Es256SigningKey::generate();
                let pem = key.to_pkcs8_pem().map_err(|e| {
                    CliError::State(format!("could not serialize receipt key: {e}"))
                })?;
                write_private(&path, pem.as_bytes())?;
                Ok(key)
            }
            Err(e) => Err(CliError::Io(e)),
        }
    }

    /// Load the Cedar policy bundle from [`cedar_path`](Self::cedar_path) if it
    /// exists. When absent, load a deny-all policy unless the operator has
    /// explicitly enabled the local-development fallback with
    /// `ARDUR_DEV_PERMISSIVE_POLICY=true`.
    pub fn load_cedar_policies(&self) -> Result<CedarPolicyBundle, CliError> {
        self.load_cedar_policies_with_dev_fallback(dev_permissive_policy_enabled())
    }

    /// Load Cedar policies with the caller-supplied dev-mode decision. Exposed so
    /// tests can verify the security default without mutating process-global
    /// environment variables.
    pub fn load_cedar_policies_with_dev_fallback(
        &self,
        dev_permissive_policy: bool,
    ) -> Result<CedarPolicyBundle, CliError> {
        let path = self.cedar_path();
        let source = if path.exists() {
            PolicySource::File(path)
        } else if dev_permissive_policy {
            PolicySource::Embedded(PERMISSIVE_POLICY.to_string())
        } else {
            PolicySource::Embedded(DENY_ALL_POLICY.to_string())
        };
        CedarPolicyBundle::load(source)
            .map_err(|e| CliError::State(format!("loading Cedar policies: {e}")))
    }

    /// The Cedar/cost-gate subject this local session authorizes as:
    /// `cli://localhost-<uid>`. The uid is the owner of the home directory (a
    /// safe stand-in for `getuid(2)` that needs no `unsafe` FFI); when that is
    /// unavailable (non-unix, unreadable home) it falls back to `$USER` and then
    /// `anonymous`.
    #[must_use]
    pub fn local_subject(&self) -> String {
        format!("cli://localhost-{}", local_uid(&self.root))
    }
}

/// Whether the explicit local-development permissive Cedar fallback is enabled.
fn dev_permissive_policy_enabled() -> bool {
    std::env::var("ARDUR_DEV_PERMISSIVE_POLICY")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Write `bytes` to `path`, owner-read/write only on unix (`0o600`) so private
/// key material is not group/world readable.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// The local user id used in the session subject. See [`StateDirs::local_subject`].
fn local_uid(home: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        // The home dir is owned by the running user, so its owner uid is the
        // process uid in practice — without reaching for `unsafe` libc FFI.
        if let Ok(meta) = std::fs::metadata(home) {
            return meta.uid().to_string();
        }
    }
    let _ = home;
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "anonymous".to_string())
}

#[cfg(test)]
mod tests {
    use ardur_cedar_policy::{
        ActionRef, Decision, EvaluationContext, PolicyBundle, PrincipalRef, ResourceRef,
    };
    use serde_json::Value;

    use super::*;

    fn state_under(home: &Path) -> StateDirs {
        StateDirs {
            root: home.join(".ardur"),
            memory: home.join(".ardur/memory"),
            journals: home.join(".ardur/journals"),
            receipts: home.join(".ardur/receipts"),
            keys: home.join(".ardur/keys"),
        }
    }

    fn eval_chat_submit(bundle: &CedarPolicyBundle) -> Decision {
        bundle.evaluate(&EvaluationContext {
            principal: PrincipalRef("User::\"cli://test\"".to_string()),
            action: ActionRef("Action::Submit".to_string()),
            resource: ResourceRef("Session::\"local\"".to_string()),
            attributes: Value::Null,
        })
    }

    #[test]
    fn missing_cli_cedar_policy_denies_by_default() {
        let home = tempfile::tempdir().expect("temp home");
        let state = state_under(home.path());

        let bundle = state
            .load_cedar_policies_with_dev_fallback(false)
            .expect("missing policy loads deny-all fallback");
        assert!(matches!(eval_chat_submit(&bundle), Decision::Deny { .. }));
    }

    #[test]
    fn explicit_dev_permissive_policy_allows_missing_cli_policy() {
        let home = tempfile::tempdir().expect("temp home");
        let state = state_under(home.path());

        let bundle = state
            .load_cedar_policies_with_dev_fallback(true)
            .expect("explicit dev fallback loads permissive policy");

        assert!(matches!(eval_chat_submit(&bundle), Decision::Allow { .. }));
    }
}
