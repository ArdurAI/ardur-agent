//! Grant writes share the receipt-chain lease with runtimes and approval writers.
//! These tests run the shipped CLI, with real signing keys and file-backed owners.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ardur_cedar_policy::{CedarPolicyBundle, PolicyBundle, PolicySource};
use ardur_cli::{StateDirs, grants_path, read_grant_records};
use ardur_fused_runtime::{
    ControlReceiptWriter, FusedRuntime, FusedRuntimeBuilder, PersistedReceipt,
    load_persisted_chain, verify_persisted_chain_with_jwks,
};
use ardur_provider_runtime::{AnthropicProvider, ModelId};
use ardur_receipt::{
    CostTuple, Es256SigningKey, HolderId, Jwks, Sha256Digest, TokenId, VerbObject,
};
use assert_cmd::Command;
use serde_json::{Value, json};

fn ardur(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ardur").expect("the `ardur` binary builds");
    // No ambient ARDUR/provider credentials, configuration, or working directory.
    cmd.env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .current_dir(home)
        .timeout(Duration::from_secs(15));
    // Preserve only the identity fallback used by StateDirs on non-Unix hosts.
    for name in ["USER", "USERNAME", "SystemRoot"] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    cmd
}

fn read_optional(path: &Path) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("reading {}: {error}", path.display()),
    }
}

struct Fixture {
    _home_guard: tempfile::TempDir,
    home: PathBuf,
    dirs: StateDirs,
    key: Es256SigningKey,
}

struct GrantState {
    log: Vec<u8>,
    ledger: Option<Vec<u8>>,
    records: Value,
}

impl Fixture {
    fn new(seed_grant: bool) -> Self {
        let home_guard = tempfile::tempdir().expect("isolated HOME");
        let home = home_guard.path().canonicalize().expect("canonical HOME");
        let root = home.join(".ardur");
        let dirs = StateDirs {
            memory: root.join("memory"),
            journals: root.join("journals"),
            receipts: root.join("receipts"),
            keys: root.join("keys"),
            root,
        };
        dirs.create().expect("isolated state tree");
        let key = dirs.load_or_create_receipt_key().expect("real receipt key");
        if seed_grant {
            ardur(&home)
                .args(["grant", "allow", "shell.run", "--scope", "echo"])
                .assert()
                .success();
        }
        Self {
            _home_guard: home_guard,
            home,
            dirs,
            key,
        }
    }

    fn grant_command(&self) -> Command {
        let mut cmd = ardur(&self.home);
        cmd.args(["grant", "allow", "http.fetch", "--scope", "example.test"]);
        cmd
    }

    fn snapshot(&self) -> GrantState {
        GrantState {
            // Builder preflight may create an empty receipt log before any receipt.
            log: read_optional(&self.dirs.receipt_log()).unwrap_or_default(),
            ledger: read_optional(&grants_path(&self.dirs)),
            records: serde_json::to_value(read_grant_records(&self.dirs).unwrap()).unwrap(),
        }
    }

    fn authenticated_chain(&self) -> Vec<PersistedReceipt> {
        let chain = load_persisted_chain(self.dirs.receipt_log()).expect("receipt chain loads");
        verify_persisted_chain_with_jwks(&chain, &Jwks::from_public_key(&self.key.public_key()))
            .expect("entire chain authenticates with the installation key");
        chain
    }

    fn runtime_owner(&self) -> FusedRuntime {
        let model = ModelId::new("grant-writer-fixture");
        FusedRuntimeBuilder::new(
            ardur_cap_token::KeyPair::new().public(),
            CedarPolicyBundle::load(PolicySource::Embedded(
                "permit(principal, action, resource);".into(),
            ))
            .unwrap(),
            Arc::new(AnthropicProvider::stub(model.clone())),
            self.key.clone(),
            model,
        )
        .receipt_log(self.dirs.receipt_log())
        .build()
        .expect("production runtime owns the file-backed writer lease")
    }

    fn assert_grant_refused(&self, before: &GrantState) {
        let output = self.grant_command().output().expect("grant command runs");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "overlapping grant must fail: {stderr}"
        );
        assert!(stderr.contains("writer already active"), "{stderr}");
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("granted `"),
            "writer contention must not report a successful grant"
        );
        let after = self.snapshot();
        assert_eq!(after.log, before.log, "no signed receipt may be appended");
        assert_eq!(after.ledger, before.ledger, "no grant ledger mutation");
        assert_eq!(after.records, before.records, "no new capability authority");
        // Read-only grant inspection remains available while another writer owns the log.
        ardur(&self.home).args(["grant", "list"]).assert().success();
    }

    fn assert_grant_succeeds(&self) {
        let before = self.snapshot();
        let previous_chain = self.authenticated_chain();
        let output = self
            .grant_command()
            .output()
            .expect("same grant command runs");
        assert!(
            output.status.success(),
            "grant after release failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let chain = self.authenticated_chain();
        assert_eq!(chain.len(), previous_chain.len() + 1);
        assert!(
            self.snapshot().log.starts_with(&before.log),
            "existing signed bytes changed"
        );
        let receipt = &chain.last().unwrap().body;
        assert_eq!(
            receipt.parent_hash,
            previous_chain
                .last()
                .map(|r| Sha256Digest::of(r.jws_compact.as_bytes()))
        );

        let records = read_grant_records(&self.dirs).expect("persisted grant records load");
        let previous_records = before.records.as_array().unwrap();
        assert_eq!(records.len(), previous_records.len() + 1);
        assert_eq!(
            serde_json::to_value(&records[..previous_records.len()]).unwrap(),
            before.records,
            "prior grants must survive unchanged"
        );
        let grant = records.last().unwrap();
        assert_eq!(grant.tool, "http.fetch");
        assert_eq!(grant.capabilities, vec!["cap.network_out"]);
        assert_eq!(grant.scope.as_deref(), Some("example.test"));
        assert_eq!(grant.subject, self.dirs.local_subject());
        assert!(grant.granted_at_ms > 0);
        assert_eq!(grant.receipt_id, Some(receipt.receipt_id.to_string()));
        assert_eq!(receipt.verb.as_str(), "tool.grant.allow.v1");
        assert_eq!(receipt.subject, HolderId(grant.subject.clone()));
        assert_eq!(receipt.issued_at.0, grant.granted_at_ms);
        assert_eq!(
            receipt.cap_token_id,
            TokenId(uuid::Uuid::from_bytes(*b"ardur-op-grant!!"))
        );
        let payload = json!({
            "tool": grant.tool,
            "capabilities": grant.capabilities,
            "scope": grant.scope,
            "subject": grant.subject,
            "granted_at_ms": grant.granted_at_ms,
        });
        assert_eq!(
            receipt.payload_digest,
            Sha256Digest::of(&serde_json::to_vec(&payload).unwrap())
        );
        assert_eq!(receipt.session_id, None);
        assert_eq!(receipt.cost, CostTuple::default());
        assert!(receipt.tool_calls.is_empty());
        assert_eq!(receipt.provider, None);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("granted `http.fetch`"), "{stdout}");
        assert!(stdout.contains(&receipt.receipt_id.to_string()), "{stdout}");
        ardur(&self.home)
            .args(["receipts", "verify"])
            .assert()
            .success();
    }
}

fn assert_runtime_contention(seed_grant: bool) {
    let fixture = Fixture::new(seed_grant);
    let runtime = fixture.runtime_owner();
    let supervisor = runtime.settlement_supervisor();
    let before = fixture.snapshot();
    if seed_grant {
        assert_eq!(fixture.authenticated_chain().len(), 1);
        assert_eq!(before.records.as_array().unwrap().len(), 1);
    } else {
        assert!(before.log.is_empty());
        assert!(before.records.as_array().unwrap().is_empty());
    }
    fixture.assert_grant_refused(&before);
    drop(runtime);
    // The retained supervisor is still the real owner, even without a live runtime.
    fixture.assert_grant_refused(&before);
    assert!(
        supervisor.try_close().is_ok(),
        "empty supervisor releases safely"
    );
    fixture.assert_grant_succeeds();
}

#[test]
fn grant_allow_refuses_live_and_retained_runtime_before_mutating_grants() {
    assert_runtime_contention(true);
}

#[test]
fn first_grant_refuses_runtime_with_empty_preflight_log_then_succeeds() {
    assert_runtime_contention(false);
}

fn assert_approval_writer_contention(verb: &str) {
    let fixture = Fixture::new(true);
    let writer = ControlReceiptWriter::open(&fixture.dirs.receipt_log(), &fixture.key)
        .expect("approval writer acquires the same stable lease");
    let before = fixture.snapshot();
    assert_eq!(fixture.authenticated_chain().len(), 1);
    fixture.assert_grant_refused(&before);
    // Finish the owning approval write through the same consuming API used by
    // both CLI approval decisions. The subsequent grant must use this NEW tail.
    let approval = writer
        .mint(
            VerbObject::new(verb).unwrap(),
            Sha256Digest::of(b"grant-writer-approval"),
            HolderId("local-operator".into()),
            TokenId(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, b"cli-local")),
            None,
            CostTuple::default(),
            1_000,
        )
        .expect("approval mint consumes and releases the owner");
    let chain = fixture.authenticated_chain();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[1].body, approval);
    assert_eq!(chain[1].body.verb.as_str(), verb);
    assert_eq!(fixture.snapshot().ledger, before.ledger);
    fixture.assert_grant_succeeds();
}

#[test]
fn grant_allow_refuses_approval_approve_writer_then_continues_its_chain() {
    assert_approval_writer_contention("approval.approve.accepted.v1");
}

#[test]
fn grant_allow_refuses_approval_deny_writer_then_continues_its_chain() {
    assert_approval_writer_contention("approval.reject.accepted.v1");
}
