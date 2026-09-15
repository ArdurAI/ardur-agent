//! File protocol regressions. Fixtures never log bearer tokens or key material.

use std::fs;
use std::io;

use ardur_cap_token::{
    AttenuationRule, BiscuitCapTokenAttenuator, BiscuitCapTokenIssuer, BiscuitCapTokenVerifier,
    CapScope, CapToken, CapTokenAttenuator, CapTokenError, CapTokenIssuer, CapTokenVerifier,
    DenyList, FileDenyList, HolderId, KeyPair, PublicKey, RequiredCaveats,
};
use biscuit_auth::builder::Algorithm;

mod support;

#[cfg(target_os = "macos")]
#[path = "support/fault_tests.rs"]
mod fault_tests;

// Only the parent tests invoke this entry point in fresh OS processes.
#[test]
#[ignore = "subprocess entry point"]
fn file_worker() {
    support::worker();
}

#[test]
fn two_writer_processes_fresh_verifier_checks_every_ack() {
    let dir = tempfile::tempdir().expect("tempdir");
    let count = 32;
    support::fixture(dir.path(), count);
    let first = support::Worker::spawn(dir.path(), "writer", 0, None);
    let second = support::Worker::spawn(dir.path(), "writer", 1, None);
    for slot in 0..2 {
        support::wait_for(&dir.path().join(format!("writer-{slot}.ready")));
    }
    support::mark(dir.path(), "go");
    assert!(first.finish().success(), "first writer process failed");
    assert!(second.finish().success(), "second writer process failed");
    let acknowledgments = fs::read_dir(dir.path())
        .expect("list markers")
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("ack-")
        })
        .count();
    assert_eq!(
        acknowledgments, count,
        "account for every successful revoke"
    );
    let verifier = support::Worker::spawn(dir.path(), "verifier", 0, None);
    assert!(
        verifier.finish().success(),
        "third, fresh verifier process failed"
    );
}

#[test]
fn writer_waits_for_independently_opened_shared_lock() {
    use std::fs::{OpenOptions, TryLockError};
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("tempdir");
    support::fixture(dir.path(), 2);
    let mut writer = support::Worker::spawn(dir.path(), "writer", 0, None);
    support::wait_for(&dir.path().join("writer-0.ready"));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.path().join("deny.hex"))
        .expect("independent handle");
    lock.lock_shared().expect("hold shared lock");
    let other = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.path().join("deny.hex"))
        .expect("second independent handle");
    assert!(
        matches!(other.try_lock(), Err(TryLockError::WouldBlock)),
        "std locks must actually contend across independently opened handles"
    );
    other.lock_shared().expect("shared readers can coexist");
    drop(other);
    support::mark(dir.path(), "go");
    support::wait_for(&dir.path().join("writer-0.entered"));
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        writer.still_running(),
        "writer bypassed another process's shared file lock"
    );
    assert!(
        !dir.path().join("ack-0").exists(),
        "no acknowledgment before lock release"
    );
    drop(lock);
    assert!(
        writer.finish().success(),
        "writer resumes after lock release"
    );
}

fn read_operation_waits_for_complete_record(role: &str) {
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("tempdir");
    support::fixture(dir.path(), 2);
    let mut reader = support::Worker::spawn(dir.path(), role, 0, None);
    support::wait_for(&dir.path().join(format!("{role}-0.ready")));
    let mut lock = OpenOptions::new()
        .read(true)
        .append(true)
        .open(dir.path().join("deny.hex"))
        .expect("independent handle");
    lock.lock().expect("hold exclusive writer lock");
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let id = hex::encode(issue(&issuer).revocation_ids().remove(0));
    lock.write_all(&id.as_bytes()[..64])
        .expect("partial record under lock");
    support::mark(dir.path(), "go");
    support::wait_for(&dir.path().join(format!("{role}-0.entered")));
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        reader.still_running(),
        "reader/open bypassed another process's exclusive file lock"
    );
    lock.write_all(&id.as_bytes()[64..])
        .expect("finish payload");
    lock.write_all(b"\n").expect("finish record");
    lock.sync_all().expect("sync before unlocking");
    drop(lock);
    assert!(
        reader.finish().success(),
        "reader resumes on complete valid snapshot"
    );
}

#[test]
fn reader_waits_for_independent_writer() {
    read_operation_waits_for_complete_record("reader");
}

#[test]
fn open_waits_for_independent_writer() {
    read_operation_waits_for_complete_record("opener");
}

fn issue(issuer: &BiscuitCapTokenIssuer) -> CapToken {
    issuer
        .issue(
            HolderId("file-protocol-test".into()),
            CapScope {
                audience: "file-protocol-test".into(),
                expires_unix: 2_000_000_000,
                budget_remaining: 100,
                tool_allowlist: vec!["search".into()],
            },
        )
        .expect("issue local fixture")
}

fn request() -> RequiredCaveats {
    RequiredCaveats {
        now_unix: 1_700_000_000,
        audience: "file-protocol-test".into(),
        tool: "search".into(),
        cost: 1,
    }
}

fn child(parent: &CapToken) -> CapToken {
    BiscuitCapTokenAttenuator
        .attenuate(parent, AttenuationRule::ReduceBudget(50).into())
        .expect("attenuate local fixture")
}

fn assert_denied(deny: FileDenyList, token: &CapToken, root: &PublicKey) {
    assert!(
        matches!(
            BiscuitCapTokenVerifier::new(deny).verify(token, root, &request()),
            Err(CapTokenError::Revoked)
        ),
        "revoked parent or descendant must not verify"
    );
}

fn assert_corrupt_fails_closed(contents: &[u8]) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deny.hex");
    let reader = FileDenyList::open(&path).expect("open empty list");
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let token = issue(&issuer);
    assert!(!reader.is_revoked(&token.revocation_ids()));
    fs::write(&path, contents).expect("install corrupt fixture");
    assert!(
        reader.is_revoked(&token.revocation_ids()),
        "malformed framing must fail closed, not silently accept a different id"
    );
    assert_eq!(
        FileDenyList::open(&path)
            .expect_err("reject corrupt list")
            .kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn rejects_concatenated_records() {
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let a = issue(&issuer).revocation_ids().remove(0);
    let b = issue(&issuer).revocation_ids().remove(0);
    assert!(a != b, "independent real tokens must have distinct ids");
    // The possible payload-A / payload-B / newline-A / newline-B schedule.
    let concatenated = format!("{}{}\n\n", hex::encode(a), hex::encode(b));
    assert_corrupt_fails_closed(concatenated.as_bytes());
}

#[test]
fn rejects_torn_records() {
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let id = hex::encode(issue(&issuer).revocation_ids().remove(0));
    // Both a complete payload without its terminator and an even-hex prefix.
    assert_corrupt_fails_closed(id.as_bytes());
    assert_corrupt_fails_closed(&id.as_bytes()[..id.len() / 2]);
}

#[test]
fn rejects_blank_or_non_signature_records() {
    for bad in [
        b"\n".as_slice(),
        b"abcd\n",
        b"zz\n",
        b" \r\n",
        &[0xff, b'\n'],
    ] {
        assert_corrupt_fails_closed(bad);
    }
}

#[test]
fn missing_file_is_not_recreated_by_revoke() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deny.hex");
    let deny = FileDenyList::open(&path).expect("open");
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let first = issue(&issuer);
    deny.revoke_token(&first)
        .expect("acknowledge first revocation");
    fs::remove_file(&path).expect("remove fixture");
    assert!(
        deny.is_revoked(&first.revocation_ids()),
        "missing file must fail closed"
    );
    let result = deny.revoke_token(&issue(&issuer));
    assert!(
        result.is_err(),
        "revoke must not recreate a removed list and erase prior deny state"
    );
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
    assert!(
        !path.exists(),
        "failed revocation must not recreate the list"
    );
}

#[test]
fn revoke_rejects_corrupt_prefix_without_appending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deny.hex");
    let deny = FileDenyList::open(&path).expect("open");
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let torn = hex::encode(issue(&issuer).revocation_ids().remove(0));
    fs::write(&path, &torn).expect("install torn prefix");
    let result = deny.revoke_token(&issue(&issuer));
    assert!(
        result.is_err(),
        "revoke must not acknowledge an append after a torn prefix"
    );
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert!(
        fs::read(&path).expect("read") == torn.as_bytes(),
        "corrupt file must remain untouched"
    );
}

#[test]
fn invalid_id_cannot_poison_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("deny.hex");
    let deny = FileDenyList::open(&path).expect("open");
    for id in [vec![], vec![0; 32], vec![0; 65], vec![0; 128]] {
        let result = deny.revoke(id);
        assert!(
            result.is_err(),
            "invalid input must not be acknowledged or appended"
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
    assert!(fs::read(&path).expect("read").is_empty());
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    deny.revoke_token(&issue(&issuer))
        .expect("valid input still works");
}

#[test]
fn existing_ed25519_and_p256_hex_lines_remain_compatible() {
    for algorithm in [Algorithm::Ed25519, Algorithm::Secp256r1] {
        let issuer = BiscuitCapTokenIssuer::new(KeyPair::new_with_algorithm(algorithm));
        let root = issuer.public_key();
        let parent = issue(&issuer);
        let descendant = child(&parent);
        let live = issue(&issuer);
        let ids = parent.revocation_ids();
        if algorithm == Algorithm::Ed25519 {
            assert_eq!(ids[0].len(), 64);
        } else {
            assert_eq!(
                ids[0][0], 0x30,
                "P-256 uses a DER sequence, not a fixed id length"
            );
            assert_eq!(usize::from(ids[0][1]) + 2, ids[0].len());
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deny.hex");
        // Preserve the original hex-line protocol, including uppercase and CRLF.
        fs::write(&path, format!("{}\r\n", hex::encode_upper(&ids[0])))
            .expect("write existing-format record");
        assert_denied(
            FileDenyList::open(&path).expect("open legacy"),
            &parent,
            &root,
        );
        assert_denied(
            FileDenyList::open(&path).expect("open legacy"),
            &descendant,
            &root,
        );
        assert!(
            BiscuitCapTokenVerifier::new(FileDenyList::open(&path).expect("open legacy"))
                .verify(&live, &root, &request())
                .is_ok(),
            "compatibility must not be a fail-closed false positive"
        );
    }
}
