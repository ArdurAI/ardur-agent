//! macOS OS-boundary fault proofs; no production types or I/O are mocked.

use std::fs::{self, OpenOptions, TryLockError};
use std::io;
use std::time::{Duration, Instant};

use ardur_cap_token::{BiscuitCapTokenIssuer, DenyList, FileDenyList, KeyPair};

use super::support::{self, Worker};

fn fault_worker(mode: &str, role: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    support::fixture(dir.path(), 1);
    let library = support::fault_library(dir.path());
    let worker = Worker::spawn(dir.path(), role, 0, Some((&library, mode)));
    support::wait_for(&dir.path().join(format!("{role}-0.ready")));
    support::mark(dir.path(), "go");
    assert!(
        worker.finish().success(),
        "fault worker must reach its intended I/O assertion"
    );
    assert!(
        dir.path().join("fault.hit").exists(),
        "fault must hit the actual backing-file syscall"
    );
    dir
}

#[test]
fn append_error_propagates() {
    let dir = fault_worker("write-eio", "write-error");
    assert!(fs::read(dir.path().join("deny.hex")).unwrap().is_empty());
    assert!(!dir.path().join("ack-0").exists());
}

#[test]
fn sync_error_propagates_after_real_append() {
    let dir = fault_worker("sync-eio", "write-error");
    let contents = fs::read(dir.path().join("deny.hex")).unwrap();
    assert!(
        !contents.is_empty(),
        "exercise sync failure after real bytes were appended"
    );
    assert_eq!(contents.last(), Some(&b'\n'));
    assert!(!dir.path().join("ack-0").exists());
    // A failed sync does not promise rollback: the complete record may be visible.
    FileDenyList::open(dir.path().join("deny.hex")).expect("read complete unsynced record");
}

#[test]
fn partial_append_error_leaves_fail_closed_tail() {
    let dir = fault_worker("partial-eio", "write-error");
    let path = dir.path().join("deny.hex");
    let contents = fs::read(&path).unwrap();
    assert!(
        !contents.is_empty() && !contents.ends_with(b"\n"),
        "inject a real partial write followed by EIO"
    );
    assert_eq!(
        FileDenyList::open(&path)
            .expect_err("torn tail must fail closed")
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert!(!dir.path().join("ack-0").exists());
}

#[test]
fn read_and_lock_errors_fail_closed() {
    for mode in ["read-eio", "lock-eio"] {
        let dir = fault_worker(mode, "read-error");
        let deny =
            FileDenyList::open(dir.path().join("deny.hex")).expect("fault is scoped to child");
        let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
        assert!(
            !deny.is_revoked(&super::issue(&issuer).revocation_ids()),
            "no permanent deny-all substitution"
        );
    }
    fault_worker("lock-eio", "write-error");
}

#[test]
fn exclusive_lock_covers_every_short_write() {
    let dir = fault_worker("short-write", "writer");
    assert!(
        !dir.path().join("write.unlocked").exists(),
        "every short write must retain the exclusive file lock"
    );
    assert!(dir.path().join("ack-0").exists());
    let verifier = Worker::spawn(dir.path(), "verifier", 0, None);
    assert!(
        verifier.finish().success(),
        "fresh verifier must see the entire acknowledged record"
    );
}

#[test]
fn exclusive_lock_is_held_until_sync_completes() {
    let dir = tempfile::tempdir().expect("tempdir");
    support::fixture(dir.path(), 1);
    let library = support::fault_library(dir.path());
    let mut worker = Worker::spawn(dir.path(), "writer", 0, Some((&library, "sync-pause")));
    support::wait_for(&dir.path().join("writer-0.ready"));
    support::mark(dir.path(), "go");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !dir.path().join("sync.entered").exists() {
        assert!(
            worker.still_running(),
            "revoke returned without reaching the real sync boundary"
        );
        assert!(Instant::now() < deadline, "sync hook was not reached");
        std::thread::sleep(Duration::from_millis(5));
    }
    let probe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.path().join("deny.hex"))
        .expect("independently opened lock probe");
    assert!(
        matches!(probe.try_lock_shared(), Err(TryLockError::WouldBlock)),
        "exclusive lock was released before sync completed"
    );
    assert!(
        !dir.path().join("ack-0").exists(),
        "success cannot precede sync completion"
    );
    support::mark(dir.path(), "sync.release");
    assert!(
        worker.finish().success(),
        "writer completes after real sync"
    );
    assert!(dir.path().join("ack-0").exists());
    let verifier = Worker::spawn(dir.path(), "verifier", 0, None);
    assert!(verifier.finish().success());
}
