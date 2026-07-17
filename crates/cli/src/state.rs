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
use serde::{Deserialize, Serialize};

use crate::CliError;
use crate::secure_io::{
    create_private_file_no_follow, read_file_no_follow, read_string_no_follow,
    write_private_file_atomic_no_follow,
};

/// The deny-all Cedar bundle used when no `cedar.policies` file is present.
/// Operators must either provide a real policy file or opt into the explicit
/// development escape hatch (`ARDUR_DEV_PERMISSIVE_POLICY=true`).
const DENY_ALL_POLICY: &str = "forbid(principal, action, resource);";

/// Explicit local-development fallback for ad-hoc CLI smoke tests.
const PERMISSIVE_POLICY: &str = "permit(principal, action, resource);";

/// Operator-facing metadata recorded alongside a durable session journal.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SessionMetadata {
    /// Durable session UUID.
    pub session_id: String,
    /// First time this metadata file was written, in Unix milliseconds.
    pub created_at_ms: u64,
    /// Most recent time this session was opened, in Unix milliseconds.
    pub updated_at_ms: u64,
    /// Provider selected for the session.
    pub provider: String,
    /// Requested model selected for the session.
    pub model: String,
    /// Ingress surface that created or opened the session.
    pub source: String,
    /// Workspace name captured when the session was first created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

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
        match read_file_no_follow(&path) {
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
                create_private_file_no_follow(&path, &keypair.private().to_bytes())?;
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
        match read_string_no_follow(&path) {
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
                create_private_file_no_follow(&path, pem.as_bytes())?;
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

    /// Create or update the operator metadata stored beside a session journal.
    pub fn record_session_metadata(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
        source: &str,
    ) -> Result<(), CliError> {
        let session_id = uuid::Uuid::parse_str(session_id)
            .map(|id| id.to_string())
            .map_err(|_| CliError::State("session metadata id must be a valid UUID".to_string()))?;
        let path = self
            .journals
            .join("sessions")
            .join(&session_id)
            .join("metadata.json");
        let existing = match read_string_no_follow(&path) {
            Ok(raw) => {
                let metadata = serde_json::from_str::<SessionMetadata>(&raw).map_err(|error| {
                    CliError::State(format!(
                        "parsing existing session metadata {}: {error}",
                        path.display()
                    ))
                })?;
                if metadata.session_id != session_id {
                    return Err(CliError::State(format!(
                        "session metadata id mismatch: expected `{session_id}`, found `{}`",
                        metadata.session_id
                    )));
                }
                Some(metadata)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(CliError::Io(error)),
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let metadata = SessionMetadata {
            session_id: session_id.to_string(),
            created_at_ms: existing
                .as_ref()
                .map(|metadata| metadata.created_at_ms)
                .filter(|created_at| *created_at > 0)
                .unwrap_or(now_ms),
            updated_at_ms: now_ms,
            provider: provider.to_string(),
            model: model.to_string(),
            source: source.to_string(),
            workspace: existing
                .as_ref()
                .and_then(|metadata| metadata.workspace.clone())
                .or_else(current_workspace_name),
        };
        let bytes = serde_json::to_vec_pretty(&metadata)
            .map_err(|error| CliError::State(format!("serializing session metadata: {error}")))?;
        write_private_file_atomic_no_follow(&path, &bytes)?;
        Ok(())
    }
}

fn current_workspace_name() -> Option<String> {
    std::env::current_dir()
        .ok()?
        .file_name()?
        .to_str()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
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
            .expect("missing policy loads explicit dev fallback");
        assert!(matches!(eval_chat_submit(&bundle), Decision::Allow { .. }));
    }

    #[test]
    fn session_metadata_rejects_non_uuid_path_components() {
        let home = tempfile::tempdir().expect("temp home");
        let state = state_under(home.path());

        let error = state
            .record_session_metadata("../escape", "provider", "model", "cli")
            .expect_err("session path traversal must be rejected");

        assert!(error.to_string().contains("valid UUID"), "{error}");
        assert!(!state.journals.join("escape/metadata.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_key_and_metadata_loaders_reject_symlinks() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().expect("temp home");
        let home_path = home.path().canonicalize().expect("canonical temp home");
        let state = state_under(&home_path);
        std::fs::create_dir_all(&state.keys).expect("keys directory");
        let key_target = home_path.join("outside-receipt.pem");
        std::fs::write(&key_target, "not a private key").expect("key target");
        symlink(&key_target, state.receipt_key_path()).expect("receipt key symlink");
        let error = match state.load_or_create_receipt_key() {
            Ok(_) => panic!("receipt key symlink must fail closed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("symlink"), "{error}");

        let session_id = uuid::Uuid::new_v4().to_string();
        let session_dir = state.journals.join("sessions").join(&session_id);
        std::fs::create_dir_all(&session_dir).expect("session directory");
        let metadata_target = home_path.join("outside-metadata.json");
        std::fs::write(
            &metadata_target,
            format!(
                r#"{{"session_id":"{session_id}","created_at_ms":1,"updated_at_ms":1,"provider":"p","model":"m","source":"cli"}}"#
            ),
        )
        .expect("metadata target");
        symlink(&metadata_target, session_dir.join("metadata.json")).expect("metadata symlink");
        let error = state
            .record_session_metadata(&session_id, "p", "m", "cli")
            .expect_err("metadata symlink must fail closed");
        assert!(error.to_string().contains("symlink"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn session_metadata_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let home = tempfile::tempdir().expect("temp home");
        let home_path = home.path().canonicalize().expect("canonical temp home");
        let state = state_under(&home_path);
        let session_id = uuid::Uuid::new_v4().to_string();
        state
            .record_session_metadata(&session_id, "provider", "model", "cli")
            .expect("metadata write");

        let session_dir = state.journals.join("sessions").join(session_id);
        let mode = std::fs::metadata(session_dir.join("metadata.json"))
            .expect("metadata stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(
            std::fs::read_dir(&session_dir)
                .expect("session directory")
                .all(|entry| !entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".metadata-")),
            "atomic rename must not leave temporary metadata files"
        );
    }
}
