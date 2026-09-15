//! Real-process fixtures and bounded child cleanup for the file protocol tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ardur_cap_token::{
    BiscuitCapTokenIssuer, BiscuitCapTokenVerifier, CapToken, CapTokenError, CapTokenVerifier,
    DenyList, FileDenyList, KeyPair, PublicKey,
};
use biscuit_auth::builder::Algorithm;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Family {
    p256: bool,
    root: String,
    parent: String,
    child: String,
    grandchild: String,
    live: String,
}

impl Family {
    fn root(&self) -> PublicKey {
        let algorithm = if self.p256 {
            Algorithm::Secp256r1
        } else {
            Algorithm::Ed25519
        };
        PublicKey::from_bytes_hex(&self.root, algorithm).expect("parse fixture public key")
    }

    fn token(&self, wire: &str) -> CapToken {
        CapToken::from_base64(wire, &self.root()).expect("parse local token fixture")
    }
}

pub fn fixture(dir: &Path, count: usize) {
    let families: Vec<_> = (0..count)
        .map(|index| {
            let p256 = index % 2 != 0;
            let algorithm = if p256 {
                Algorithm::Secp256r1
            } else {
                Algorithm::Ed25519
            };
            let issuer = BiscuitCapTokenIssuer::new(KeyPair::new_with_algorithm(algorithm));
            let parent = super::issue(&issuer);
            let child = super::child(&parent);
            let grandchild = super::child(&child);
            let live = super::issue(&issuer);
            let verifier = BiscuitCapTokenVerifier::new(
                FileDenyList::open(dir.join("deny.hex")).expect("open fixture list"),
            );
            for token in [&parent, &child, &grandchild, &live] {
                assert!(
                    verifier
                        .verify(token, &issuer.public_key(), &super::request())
                        .is_ok(),
                    "every fixture must verify before revocation"
                );
            }
            Family {
                p256,
                root: issuer.public_key().to_bytes_hex(),
                parent: parent.to_base64().expect("serialize fixture"),
                child: child.to_base64().expect("serialize fixture"),
                grandchild: grandchild.to_base64().expect("serialize fixture"),
                live: live.to_base64().expect("serialize fixture"),
            }
        })
        .collect();
    let unique_ids: std::collections::HashSet<_> = families
        .iter()
        .map(|family| family.token(&family.parent).revocation_ids().remove(0))
        .collect();
    assert_eq!(
        unique_ids.len(),
        count,
        "all writer fixtures have distinct real revocation ids"
    );
    fs::write(
        dir.join("fixtures.json"),
        serde_json::to_vec(&families).expect("serialize fixtures"),
    )
    .expect("write private temporary fixtures");
}

pub fn mark(dir: &Path, name: &str) {
    fs::write(dir.join(name), b"ready").expect("write coordination marker");
}

pub fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for process coordination"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

pub struct Worker {
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
    reported: bool,
}

impl Worker {
    pub fn spawn(dir: &Path, role: &str, slot: usize, injection: Option<(&Path, &str)>) -> Self {
        let name = format!("{role}-{slot}");
        let stdout = dir.join(format!("{name}.stdout"));
        let stderr = dir.join(format!("{name}.stderr"));
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--exact", "file_worker", "--ignored", "--nocapture"])
            .env_clear()
            .env("FILE_DENY_TEST_DIR", dir)
            .env("FILE_DENY_TEST_ROLE", role)
            .env("FILE_DENY_TEST_SLOT", slot.to_string())
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout).expect("worker stdout"))
            .stderr(fs::File::create(&stderr).expect("worker stderr"));
        if let Some((library, mode)) = injection {
            command.env("FILE_DENY_FAULT_MODE", mode);
            #[cfg(target_os = "macos")]
            command.env("DYLD_INSERT_LIBRARIES", library);
            #[cfg(target_os = "linux")]
            command.env("LD_PRELOAD", library);
        }
        println!("child command: {command:?}");
        let child = command.spawn().expect("spawn real process");
        println!("spawned {name}, pid={}", child.id());
        Self {
            child,
            stdout,
            stderr,
            reported: false,
        }
    }

    pub fn still_running(&mut self) -> bool {
        self.child.try_wait().expect("poll child").is_none()
    }

    pub fn finish(mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll child") {
                println!("child pid={} exited: {status}", self.child.id());
                self.report();
                return status;
            }
            assert!(Instant::now() < deadline, "worker did not finish");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn report(&mut self) {
        if !self.reported {
            for path in [&self.stdout, &self.stderr] {
                print!(
                    "{}:\n{}",
                    path.file_name().unwrap().to_string_lossy(),
                    fs::read_to_string(path).expect("read complete worker log")
                );
            }
            self.reported = true;
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        if let Ok(status) = self.child.wait()
            && !self.reported
        {
            println!("child pid={} exited: {status}", self.child.id());
        }
        self.report();
    }
}

#[cfg(target_os = "macos")]
pub fn fault_library(dir: &Path) -> PathBuf {
    let source = dir.join("file_faults.c");
    fs::write(&source, include_str!("file_faults.c")).expect("write temporary interposer source");
    let library = dir.join("file_faults.dylib");
    let output = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-dynamiclib"])
        .arg(&source)
        .arg("-o")
        .arg(&library)
        .output()
        .expect("cc is required for macOS syscall fault proofs");
    println!(
        "cc -std=c11 -Wall -Wextra -Werror -dynamiclib {} -o {}: {}",
        source.display(),
        library.display(),
        output.status
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "compile test-only syscall interposer"
    );
    library
}

pub fn worker() {
    let dir =
        PathBuf::from(std::env::var_os("FILE_DENY_TEST_DIR").expect("worker fixture directory"));
    let role = std::env::var("FILE_DENY_TEST_ROLE").expect("worker role");
    let slot: usize = std::env::var("FILE_DENY_TEST_SLOT")
        .unwrap()
        .parse()
        .unwrap();
    let name = format!("{role}-{slot}");
    let families: Vec<Family> =
        serde_json::from_slice(&fs::read(dir.join("fixtures.json")).expect("read fixtures"))
            .expect("parse fixtures");
    let deny = FileDenyList::open(dir.join("deny.hex")).expect("independently open shared path");
    mark(&dir, &format!("{name}.ready"));
    wait_for(&dir.join("go"));
    mark(&dir, &format!("{name}.entered"));
    if std::env::var_os("FILE_DENY_FAULT_MODE").is_some() {
        mark(&dir, "fault.armed");
    }
    match role.as_str() {
        "write-error" => {
            let family = &families[0];
            let result = deny.revoke_token(&family.token(&family.parent));
            assert!(
                result.is_err(),
                "append/sync/lock failure must propagate without acknowledgment"
            );
            let error = result.unwrap_err();
            assert!(
                error.raw_os_error().is_some(),
                "the actual OS I/O error must propagate"
            );
            println!("observed OS I/O error: {:?}", error.raw_os_error());
        }
        "read-error" => {
            let family = &families[0];
            assert!(
                deny.is_revoked(&family.token(&family.parent).revocation_ids()),
                "read/lock error must fail closed, not reuse a live snapshot"
            );
            assert!(
                FileDenyList::open(dir.join("deny.hex")).is_err(),
                "initial read/lock error must propagate"
            );
        }
        "writer" => {
            let mut count = 0;
            for index in (slot..families.len()).step_by(2) {
                let family = &families[index];
                deny.revoke_token(&family.token(&family.parent))
                    .expect("append and sync real revocation");
                mark(&dir, &format!("ack-{index}"));
                count += 1;
            }
            println!("acknowledged={count}");
        }
        "verifier" => {
            let verifier = BiscuitCapTokenVerifier::new(deny);
            let mut acknowledged = 0;
            let mut checked = 0;
            let mut missed = Vec::new();
            let mut rejected_controls = Vec::new();
            for (index, family) in families.iter().enumerate() {
                assert!(
                    dir.join(format!("ack-{index}")).exists(),
                    "every issued parent must have an explicit successful-revoke acknowledgment"
                );
                acknowledged += 1;
                for (generation, wire) in [&family.parent, &family.child, &family.grandchild]
                    .iter()
                    .enumerate()
                {
                    if !matches!(
                        verifier.verify(&family.token(wire), &family.root(), &super::request()),
                        Err(CapTokenError::Revoked)
                    ) {
                        missed.push((index, generation));
                    }
                    checked += 1;
                }
                if verifier
                    .verify(
                        &family.token(&family.live),
                        &family.root(),
                        &super::request(),
                    )
                    .is_err()
                {
                    rejected_controls.push(index);
                }
            }
            // Inspect EVERY acknowledgment before reporting any failure. Only
            // fixture indexes/generations are logged, never token or key bytes.
            println!(
                "verified_acknowledged={acknowledged}, checked_parent_and_descendants={checked}, missed={}, live_controls={acknowledged}, rejected_controls={}",
                missed.len(),
                rejected_controls.len()
            );
            assert!(
                missed.is_empty(),
                "fresh verifier accepted acknowledged parent/descendant indexes: {missed:?}"
            );
            assert!(
                rejected_controls.is_empty(),
                "fresh verifier must not pass by denying everything"
            );
        }
        "reader" => {
            let family = &families[1];
            assert!(
                !deny.is_revoked(&family.token(&family.parent).revocation_ids()),
                "reader must see a completed valid snapshot, not a writer's torn prefix"
            );
        }
        "opener" => {
            FileDenyList::open(dir.join("deny.hex")).expect("open must wait for a consistent file");
        }
        other => panic!("unknown worker role: {other}"),
    }
    mark(&dir, &format!("{name}.done"));
}
