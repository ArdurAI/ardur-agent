use super::*;
use ardur_core_types::Sha256Digest;
use rustix::fs::{Dir, FlockOperation, Mode, OFlags, flock, mkdirat, openat};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Format {
    version: u32,
    receipt: ReceiptIdentity,
}

fn directory(path: &Path) -> Result<File, SettlementStoreError> {
    let mut fd = File::open(if path.is_absolute() { "/" } else { "." })?;
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                fd = openat(
                    &fd,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?
                .into()
            }
            Component::RootDir | Component::CurDir => (),
            _ => {
                return Err(SettlementStoreError::Invalid(
                    SettlementProblem::IdentityChanged,
                ));
            }
        }
    }
    Ok(fd)
}

// Descriptor metadata, not a pathname preflight. Ancestors use directory()
// without this final-root policy and must be trusted and durably established.
fn private_metadata(uid: u32, mode: u32, effective_uid: u32, required: u32) -> bool {
    uid == effective_uid && mode & 0o7777 == required
}

fn require_private(file: &File, required: u32) -> Result<(), SettlementStoreError> {
    let metadata = file.metadata()?;
    if !private_metadata(
        metadata.uid(),
        metadata.mode(),
        rustix::process::geteuid().as_raw(),
        required,
    ) {
        return Err(super::types::invalid(SettlementProblem::IdentityChanged));
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<File, SettlementStoreError> {
    let file = directory(path)?;
    require_private(&file, 0o700)?;
    Ok(file)
}

fn regular(dir: &File, name: &str, flags: OFlags) -> Result<File, SettlementStoreError> {
    let file = File::from(openat(
        dir,
        name,
        flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::from_bits_truncate(0o600),
    )?);
    if !file.metadata()?.is_file() {
        return Err(SettlementStoreError::Invalid(SettlementProblem::Corrupt));
    }
    require_private(&file, 0o600)?;
    Ok(file)
}

fn read_at(dir: &File, name: &str, limit: usize) -> Result<Vec<u8>, SettlementStoreError> {
    use std::io::Read;
    let file = regular(dir, name, OFlags::RDONLY)?;
    if file.metadata()?.len() > limit as u64 {
        return Err(super::types::invalid(SettlementProblem::Bounds));
    }
    let mut bytes = Vec::new();
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(super::types::invalid(SettlementProblem::Bounds));
    }
    Ok(bytes)
}

fn validate_identity(
    identity: &ReceiptIdentity,
    limits: &SettlementLimits,
) -> Result<(), SettlementStoreError> {
    if !identity.receipt_log.is_absolute()
        || identity.receipt_log.file_name().is_none()
        || identity
            .receipt_log
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(super::types::invalid(SettlementProblem::IdentityChanged));
    }
    let text = identity
        .receipt_log
        .to_str()
        .ok_or(super::types::invalid(SettlementProblem::IdentityChanged))?;
    super::types::metadata(text, limits)
}

fn format(
    dir: &File,
    identity: &ReceiptIdentity,
    limits: &SettlementLimits,
) -> Result<(), SettlementStoreError> {
    validate_identity(identity, limits)?;
    let saved: Format =
        serde_json::from_slice(&read_at(dir, "format.json", limits.max_record_bytes)?)?;
    if saved.version != SETTLEMENT_SCHEMA_VERSION || saved.receipt != *identity {
        return Err(SettlementStoreError::Invalid(
            SettlementProblem::IdentityChanged,
        ));
    }
    Ok(())
}

#[cfg(test)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum IoFault {
    #[default]
    None,
    BeforeWrite,
    LostAck,
    LostAckVerify,
    LostAckFileSync,
    LostAckParentSync,
    OpenFormatSync,
    OpenParentSync,
}

#[derive(Clone)]
struct Attempt {
    previous: Option<EncodedSnapshot>,
    next: EncodedSnapshot,
}

// Own the acquired flock separately from its file descriptor. A descriptor
// inherited between fork and exec must not extend the logical writer lifetime.
struct WriterLease(File);

impl WriterLease {
    fn acquire(file: File) -> Result<Self, SettlementStoreError> {
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Self(file)),
            Err(rustix::io::Errno::WOULDBLOCK) => Err(SettlementStoreError::WriterBusy),
            Err(error) => Err(error.into()),
        }
    }
}

impl std::ops::Deref for WriterLease {
    type Target = File;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        // File close alone waits for every duplicate open-file description
        // reference to close. Unlock now; close remains the cleanup fallback.
        let _ = flock(&self.0, FlockOperation::Unlock);
    }
}

/// A synchronous writer holding a stable, separate lifetime lease.
/// No other receipt writer is coordinated unless it also honors this lease.
pub struct FileSettlementStore {
    root: PathBuf,
    lease: WriterLease,
    dir: File,
    identity: ReceiptIdentity,
    acknowledged: HashMap<TurnId, (u64, Sha256Digest)>,
    health: StoreHealth,
    pending: Option<Attempt>,
    #[cfg(test)]
    fault: IoFault,
    limits: SettlementLimits,
}
impl FileSettlementStore {
    /// Open a private root, creating only its final directory (parent must exist).
    /// A second independent handle/process is refused, not queued.
    pub fn open(
        root: &Path,
        identity: ReceiptIdentity,
        limits: SettlementLimits,
    ) -> Result<Self, SettlementStoreError> {
        Self::open_inner(
            root,
            identity,
            limits,
            #[cfg(test)]
            IoFault::None,
        )
    }

    fn open_inner(
        root: &Path,
        identity: ReceiptIdentity,
        limits: SettlementLimits,
        #[cfg(test)] _fault: IoFault,
    ) -> Result<Self, SettlementStoreError> {
        validate_identity(&identity, &limits)?;
        let format_bytes = super::types::encode_bounded(
            &Format {
                version: SETTLEMENT_SCHEMA_VERSION,
                receipt: identity.clone(),
            },
            limits.max_record_bytes,
        )?;
        let parent = directory(root.parent().ok_or(SettlementStoreError::Invalid(
            SettlementProblem::IdentityChanged,
        ))?)?;
        let name = root.file_name().ok_or(SettlementStoreError::Invalid(
            SettlementProblem::IdentityChanged,
        ))?;
        match mkdirat(&parent, name, Mode::from_bits_truncate(0o700)) {
            Ok(()) => parent.sync_all()?,
            Err(rustix::io::Errno::EXIST) => (),
            Err(e) => return Err(e.into()),
        }
        let dir = private_directory(root)?;
        let empty = Dir::read_from(&dir)?.try_fold(true, |empty, entry| {
            let entry = entry?;
            Ok::<_, rustix::io::Errno>(
                empty && matches!(entry.file_name().to_bytes(), b"." | b".."),
            )
        })?;
        let flags = if empty {
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL
        } else {
            OFlags::RDWR
        };
        let lease = WriterLease::acquire(regular(&dir, "writer.lock", flags)?)?;
        lease.sync_all()?;
        dir.sync_all()?;
        if empty {
            ardur_durability::create_new_atomic_no_follow(
                &root.join("format.json"),
                &format_bytes,
            )?;
        }
        format(&dir, &identity, &limits)?;
        #[cfg(test)]
        if _fault == IoFault::OpenFormatSync {
            return Err(super::types::invalid(SettlementProblem::SyncFailed));
        }
        regular(&dir, "format.json", OFlags::RDONLY)?.sync_all()?;
        dir.sync_all()?;
        #[cfg(test)]
        if _fault == IoFault::OpenParentSync {
            return Err(super::types::invalid(SettlementProblem::SyncFailed));
        }
        // Covers an existing empty directory too. Ancestors above this parent
        // are supplied by the caller and must already be durably established.
        parent.sync_all()?;
        let inventory = load_settlement_snapshot(root, &identity, &limits)?;
        let acknowledged = inventory
            .snapshots
            .iter()
            .map(|snapshot| {
                (
                    snapshot.turn().turn_id,
                    (snapshot.revision(), snapshot.digest()),
                )
            })
            .collect();
        Ok(Self {
            root: root.to_owned(),
            lease,
            dir,
            identity,
            acknowledged,
            health: StoreHealth::Healthy,
            pending: None,
            #[cfg(test)]
            fault: IoFault::None,
            limits,
        })
    }
    /// Admission health. A later unrelated operation cannot clear a failure.
    pub fn health(&self) -> StoreHealth {
        self.health
    }

    /// Compare previous revision and persist immutable bytes. Duplicate exact attempts
    /// are idempotent; conflicts quarantine the writer without changing the file.
    pub fn put_exact(&mut self, previous: Option<u64>, next: &EncodedSnapshot) -> WriteResolution {
        if previous == Some(0) {
            return self.latch(super::types::invalid(SettlementProblem::Conflict));
        }
        if let StoreHealth::Unhealthy(problem) = self.health {
            let same_attempt = self
                .pending
                .as_ref()
                .is_some_and(|attempt| attempt.next.bytes() == next.bytes())
                && previous.unwrap_or(0).checked_add(1) == Some(next.revision());
            if !same_attempt {
                return WriteResolution::Unresolved(problem);
            }
            match self.resolve_pending_inner() {
                Ok(WriteResolution::DefinitelyNotApplied) => (), // only identical bytes may now be retried
                Ok(WriteResolution::Durable(_)) => {
                    return self.resolve_exact(next.turn().turn_id, next.revision(), next.digest());
                }
                Ok(other) => return other,
                Err(error) => return self.latch(error),
            }
        }
        match self.put_inner(previous, next) {
            Ok(result) => {
                if matches!(result, WriteResolution::Durable(_)) {
                    self.acknowledged
                        .insert(next.turn().turn_id, (next.revision(), next.digest()));
                    self.pending = None;
                    self.health = StoreHealth::Healthy;
                }
                result
            }
            Err(error) => {
                let problem = match error {
                    SettlementStoreError::Invalid(p) => p,
                    _ => SettlementProblem::Io,
                };
                self.health = StoreHealth::Unhealthy(problem);
                WriteResolution::Unresolved(problem)
            }
        }
    }

    /// Resolve only an exact revision/digest; this does not repeat any budget action.
    /// A mismatching or unknown identity quarantines even a healthy writer. If
    /// no pending attempt was retained, no retry bytes or reset are invented.
    pub fn resolve_exact(
        &mut self,
        turn: TurnId,
        revision: u64,
        digest: Sha256Digest,
    ) -> WriteResolution {
        let Some(attempt) = self.pending.clone() else {
            // A healthy reopened writer may re-acknowledge retained exact evidence.
            if let StoreHealth::Unhealthy(problem) = self.health {
                return WriteResolution::Unresolved(problem);
            }
            if self.acknowledged.get(&turn) != Some(&(revision, digest)) {
                return self.latch(super::types::invalid(SettlementProblem::Conflict));
            }
            let result = self.check_integrity(None).and_then(|()| {
                EncodedSnapshot::decode(
                    read_at(
                        &self.dir,
                        &format!("{}.json", turn.0),
                        self.limits.max_record_bytes,
                    )?,
                    &self.limits,
                )
            });
            return match result {
                Ok(snapshot) => {
                    self.put_exact(revision.checked_sub(1).filter(|v| *v != 0), &snapshot)
                }
                Err(error) => self.latch(error),
            };
        };
        if attempt.next.turn().turn_id != turn
            || attempt.next.revision() != revision
            || attempt.next.digest() != digest
        {
            return self.latch(super::types::invalid(SettlementProblem::Conflict));
        }
        match self.resolve_pending_inner() {
            Ok(WriteResolution::Durable(revision)) => {
                self.acknowledged.insert(turn, (revision, digest));
                self.pending = None;
                self.health = StoreHealth::Healthy;
                WriteResolution::Durable(revision)
            }
            Ok(other) => other,
            Err(error) => self.latch(error),
        }
    }

    fn latch(&mut self, error: SettlementStoreError) -> WriteResolution {
        let problem = match error {
            SettlementStoreError::Invalid(problem) => problem,
            SettlementStoreError::Encoding(_) => SettlementProblem::Corrupt,
            _ => SettlementProblem::Io,
        };
        self.health = StoreHealth::Unhealthy(problem);
        WriteResolution::Unresolved(problem)
    }

    fn resolve_pending_inner(&self) -> Result<WriteResolution, SettlementStoreError> {
        let attempt = self
            .pending
            .as_ref()
            .ok_or(super::types::invalid(SettlementProblem::Conflict))?;
        let next = &attempt.next;
        self.check_integrity(Some(next))?;
        let observed = match read_at(
            &self.dir,
            &format!("{}.json", next.turn().turn_id.0),
            self.limits.max_record_bytes,
        ) {
            Ok(bytes) => Some(bytes),
            Err(SettlementStoreError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if observed.as_deref() == Some(next.bytes()) {
            self.sync_exact(next)?;
            return Ok(WriteResolution::Durable(next.revision()));
        }
        if observed.as_deref() == attempt.previous.as_ref().map(EncodedSnapshot::bytes) {
            return Ok(WriteResolution::DefinitelyNotApplied);
        }
        Err(super::types::invalid(SettlementProblem::VerificationFailed))
    }

    // Retain every acknowledged identity/revision during this writer lifetime.
    // This is not a durable membership index or a historical completeness proof.
    fn check_integrity(
        &self,
        allowed: Option<&EncodedSnapshot>,
    ) -> Result<(), SettlementStoreError> {
        let same_inode = |a: &File, b: &File| -> Result<bool, SettlementStoreError> {
            let a = a.metadata()?;
            let b = b.metadata()?;
            Ok(a.dev() == b.dev() && a.ino() == b.ino())
        };
        require_private(&self.dir, 0o700)?;
        require_private(&self.lease, 0o600)?;
        let dir = private_directory(&self.root)
            .map_err(|_| super::types::invalid(SettlementProblem::IdentityChanged))?;
        let lease = regular(&dir, "writer.lock", OFlags::RDONLY)
            .map_err(|_| super::types::invalid(SettlementProblem::IdentityChanged))?;
        if !same_inode(&dir, &self.dir)? || !same_inode(&lease, &self.lease)? {
            return Err(super::types::invalid(SettlementProblem::IdentityChanged));
        }
        let inventory = load_settlement_snapshot(&self.root, &self.identity, &self.limits)?;
        let mut seen = std::collections::HashSet::new();
        for snapshot in inventory.snapshots {
            let id = snapshot.turn().turn_id;
            seen.insert(id);
            if allowed
                .is_some_and(|next| next.turn().turn_id == id && next.bytes() == snapshot.bytes())
            {
                continue;
            }
            match self.acknowledged.get(&id) {
                Some(&(revision, digest))
                    if snapshot.revision() == revision && snapshot.digest() == digest => {}
                _ => return Err(super::types::invalid(SettlementProblem::Corrupt)),
            }
        }
        if self.acknowledged.keys().any(|id| !seen.contains(id)) {
            return Err(super::types::invalid(
                SettlementProblem::MissingAcknowledged,
            ));
        }
        Ok(())
    }

    fn sync_exact(&self, next: &EncodedSnapshot) -> Result<(), SettlementStoreError> {
        use std::io::Read;
        let file = regular(
            &self.dir,
            &format!("{}.json", next.turn().turn_id.0),
            OFlags::RDONLY,
        )?;
        #[cfg(test)]
        if self.fault == IoFault::LostAckVerify {
            return Err(super::types::invalid(SettlementProblem::VerificationFailed));
        }
        let mut bytes = Vec::new();
        (&file)
            .take((self.limits.max_record_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes != next.bytes() {
            return Err(super::types::invalid(SettlementProblem::VerificationFailed));
        }
        #[cfg(test)]
        if self.fault == IoFault::LostAckFileSync {
            return Err(super::types::invalid(SettlementProblem::SyncFailed));
        }
        file.sync_all()
            .map_err(|_| super::types::invalid(SettlementProblem::SyncFailed))?;
        #[cfg(test)]
        if self.fault == IoFault::LostAckParentSync {
            return Err(super::types::invalid(SettlementProblem::SyncFailed));
        }
        self.dir
            .sync_all()
            .map_err(|_| super::types::invalid(SettlementProblem::SyncFailed))?;
        self.check_integrity(Some(next))?;
        Ok(())
    }

    fn write_snapshot(&self, path: &Path, bytes: &[u8]) -> Result<(), SettlementStoreError> {
        #[cfg(test)]
        if self.fault == IoFault::BeforeWrite {
            return Err(std::io::Error::other("injected before write").into());
        }
        ardur_durability::write_atomic_no_follow(path, bytes)?;
        #[cfg(test)]
        if matches!(
            self.fault,
            IoFault::LostAck
                | IoFault::LostAckVerify
                | IoFault::LostAckFileSync
                | IoFault::LostAckParentSync
        ) {
            return Err(std::io::Error::other("injected after rename acknowledgement").into());
        }
        Ok(())
    }

    fn put_inner(
        &mut self,
        previous: Option<u64>,
        next: &EncodedSnapshot,
    ) -> Result<WriteResolution, SettlementStoreError> {
        self.check_integrity(None)?;
        super::types::validate(next.revision(), next.turn(), &self.limits)?;
        if next.bytes().len() > self.limits.max_record_bytes {
            return Err(super::types::invalid(SettlementProblem::Bounds));
        }
        if !self.acknowledged.contains_key(&next.turn().turn_id)
            && self.acknowledged.len() >= self.limits.max_inventory_records
        {
            return Err(super::types::invalid(SettlementProblem::Bounds));
        }
        if previous.unwrap_or(0).checked_add(1) != Some(next.revision()) {
            return Err(super::types::invalid(SettlementProblem::Conflict));
        }
        let path = self.root.join(format!("{}.json", next.turn().turn_id.0));
        let current = match read_at(
            &private_directory(&self.root)?,
            &format!("{}.json", next.turn().turn_id.0),
            self.limits.max_record_bytes,
        ) {
            Ok(bytes) => Some(EncodedSnapshot::decode(bytes, &self.limits)?),
            Err(SettlementStoreError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if current.as_ref().is_some_and(|v| v.bytes() == next.bytes()) {
            self.pending = Some(Attempt {
                previous: current,
                next: next.clone(),
            });
            return self.resolve_pending_inner();
        }
        if current.as_ref().map(EncodedSnapshot::revision) != previous {
            return Err(super::types::invalid(SettlementProblem::Conflict));
        }
        if let Some(current) = &current {
            super::types::transition(current.turn(), next.turn())?;
        }
        self.pending = Some(Attempt {
            previous: current,
            next: next.clone(),
        });
        if self.write_snapshot(&path, next.bytes()).is_err() {
            self.health = StoreHealth::Unhealthy(SettlementProblem::Io);
            return self.resolve_pending_inner();
        }
        self.sync_exact(next)?;
        Ok(WriteResolution::Durable(next.revision()))
    }
}

/// Load read-only evidence without acquiring a writer lease.
pub fn load_settlement_snapshot(
    root: &Path,
    identity: &ReceiptIdentity,
    limits: &SettlementLimits,
) -> Result<SettlementInventory, SettlementStoreError> {
    let dir = private_directory(root)?;
    format(&dir, identity, limits)?;
    regular(&dir, "writer.lock", OFlags::RDONLY)?;
    let mut snapshots = vec![];
    for entry in Dir::read_from(&dir)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| SettlementStoreError::Invalid(SettlementProblem::Corrupt))?;
        if matches!(name, "." | ".." | "format.json" | "writer.lock") {
            continue;
        }
        let id = name
            .strip_suffix(".json")
            .and_then(|stem| uuid::Uuid::parse_str(stem).ok())
            .ok_or(super::types::invalid(SettlementProblem::Corrupt))?;
        if name != format!("{id}.json") {
            return Err(super::types::invalid(SettlementProblem::Corrupt));
        }
        if snapshots.len() >= limits.max_inventory_records {
            return Err(super::types::invalid(SettlementProblem::Bounds));
        }
        let snapshot =
            EncodedSnapshot::decode(read_at(&dir, name, limits.max_record_bytes)?, limits)?;
        if snapshot.turn().turn_id.0 != id {
            return Err(super::types::invalid(SettlementProblem::Corrupt));
        }
        snapshots.push(snapshot);
    }
    snapshots.sort_by_key(|snapshot| snapshot.turn().turn_id.0);
    Ok(SettlementInventory {
        snapshots,
        coverage: InventoryCoverage::RetainedFilesOnly,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionId, UnixTsMillis};
    use ardur_core_types::{HolderId, TokenId};
    use uuid::Uuid;

    fn fixture() -> (tempfile::TempDir, FileSettlementStore, EncodedSnapshot) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("settlements");
        let identity = ReceiptIdentity {
            receipt_log: "/trusted/chain.jsonl".into(),
            signer: Sha256Digest::of(b"signer"),
        };
        let limits = SettlementLimits::default();
        let turn = TurnObligation {
            schema_version: SETTLEMENT_SCHEMA_VERSION,
            turn_id: TurnId(Uuid::new_v4()),
            budget_epoch: BudgetEpoch(Uuid::new_v4()),
            request_session: SessionId::new(),
            journal_owner: None,
            verified_subject: HolderId::from("caller"),
            budget_holder: HolderId::from("budget"),
            cap_token_id: TokenId(Uuid::new_v4()),
            started_at: UnixTsMillis(1),
            rounds: vec![],
            terminal: TurnTerminal::Open,
            cancellation_marker: MarkerProjection::NotRequired,
        };
        let encoded = EncodedSnapshot::new(1, turn, &limits).unwrap();
        (
            dir,
            FileSettlementStore::open(&root, identity, limits).unwrap(),
            encoded,
        )
    }

    #[test]
    fn store_drop_releases_writer_lease_while_duplicated_descriptor_lives() {
        let (_temp, mut store, snapshot) = fixture();
        let root = store.root.clone();
        let identity = store.identity.clone();
        let limits = store.limits;
        // dup retains the same open-file description, as a child can between
        // fork and exec even though the original descriptor is close-on-exec.
        let inherited = store.lease.try_clone().unwrap();
        assert!(matches!(
            FileSettlementStore::open(&root, identity.clone(), limits),
            Err(SettlementStoreError::WriterBusy)
        ));
        assert_eq!(
            store.put_exact(None, &snapshot),
            WriteResolution::Durable(1)
        );
        drop(store);
        let mut next = FileSettlementStore::open(&root, identity.clone(), limits).expect(
            "dropping the writer must release its lease before inherited descriptors close",
        );
        assert_eq!(
            next.resolve_exact(snapshot.turn().turn_id, 1, snapshot.digest()),
            WriteResolution::Durable(1)
        );
        drop(inherited);
        assert!(matches!(
            FileSettlementStore::open(&root, identity.clone(), limits),
            Err(SettlementStoreError::WriterBusy)
        ));
        drop(next);
        assert!(FileSettlementStore::open(&root, identity, limits).is_ok());
    }

    #[test]
    fn quality_q1_descriptor_owner_policy_guard() {
        let effective = rustix::process::geteuid().as_raw();
        // Unprivileged tests cannot create a foreign-owned fixture. This is
        // explicitly a pure predicate guard, not a claim of real chown coverage.
        for required in [0o700, 0o600] {
            assert!(private_metadata(effective, required, effective, required));
            assert!(!private_metadata(
                effective ^ 1,
                required,
                effective,
                required
            ));
            for extra in [0o004, 0o020, 0o1000, 0o2000, 0o4000] {
                assert!(!private_metadata(
                    effective,
                    required | extra,
                    effective,
                    required
                ));
            }
        }
        let (_temp, store, _) = fixture();
        let metadata = store.dir.metadata().unwrap();
        assert_eq!(metadata.uid(), effective);
        assert!(private_metadata(
            metadata.uid(),
            metadata.mode(),
            effective,
            0o700
        ));
    }

    #[test]
    fn spec_b2_transition_decode_and_put_reject_partial_rollback_closure() {
        use crate::CostTuple;
        let amount = |n| CostTuple {
            tokens_in: n,
            tokens_out: n,
            cents: n,
            wall_ms: n,
            attention_score: n,
        };
        let mut violations = Vec::new();
        for decision in [
            SettlementDecision::Refusal(RefusalClass::OutputBlocked),
            SettlementDecision::InfrastructureFailure(InfrastructureFailureClass::Storage),
            SettlementDecision::Cancelled,
            SettlementDecision::Completion { final_answer: true },
        ] {
            for axis in 0..5 {
                let (_temp, mut store, empty) = fixture();
                let mut credit = amount(9);
                match axis {
                    0 => credit.tokens_in = 2,
                    1 => credit.tokens_out = 2,
                    2 => credit.cents = 2,
                    3 => credit.wall_ms = 2,
                    _ => credit.attention_score = 2,
                }
                let cancelled = decision == SettlementDecision::Cancelled;
                let app = DebitApplication {
                    epoch: empty.turn().budget_epoch,
                    requested_debit: if cancelled {
                        CostTuple::ZERO
                    } else {
                        amount(12)
                    },
                    applied_debit: amount(9),
                    reserved_credit: CostTuple::ZERO,
                    additional_debit: amount(1),
                    shortfall: if cancelled {
                        CostTuple::ZERO
                    } else {
                        amount(3)
                    },
                    rollback: RollbackStatus::Applied(credit),
                };
                let mut t = empty.turn().clone();
                t.rounds.push(RoundObligation {
                    settlement_id: SettlementId(Uuid::new_v4()),
                    ordinal: 0,
                    provider_request_id: Uuid::new_v4(),
                    provider: "fixture".into(),
                    model: "paid".into(),
                    request_digest: Sha256Digest::of(b"request"),
                    reserved: amount(8),
                    provider_evidence: ProviderEvidence::Observed {
                        usage: None,
                        cost: amount(12),
                        provenance: CostProvenance::ReportedCost,
                        finished: true,
                        interrupted: false,
                    },
                    tools: vec![],
                    known_incurred: amount(12),
                    decision: Some(decision.clone()),
                    phase: SettlementPhase::Unresolved {
                        last_definite: DefinitePhase::Finalized,
                        problem: SettlementProblem::RollbackCreditPending,
                        application: Some(app.clone()),
                        candidate: None,
                    },
                    commit_ordinal: None,
                    projection: JournalProjection::NotConfigured,
                });
                t.terminal = TurnTerminal::Unresolved(SettlementProblem::RollbackCreditPending);
                let first = EncodedSnapshot::new(1, t.clone(), &store.limits).unwrap();
                assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
                t.rounds[0].phase = SettlementPhase::Settled {
                    application: app,
                    receipt: None,
                };
                t.terminal = match decision {
                    SettlementDecision::Refusal(class) => TurnTerminal::Refused(class),
                    SettlementDecision::Cancelled => TurnTerminal::Cancelled,
                    _ => TurnTerminal::Failed(InfrastructureFailureClass::Storage),
                };
                let transition = super::super::types::transition(first.turn(), &t);
                // Private construction deliberately bypasses new() so its
                // validation cannot mask missing transition/decode/put guards.
                let stored = super::super::types::StoredSnapshot {
                    revision: 2,
                    payload_digest: Sha256Digest::of(&serde_json::to_vec(&t).unwrap()),
                    payload: t,
                };
                let bytes = serde_json::to_vec(&stored).unwrap();
                let decoded = EncodedSnapshot::decode(bytes.clone(), &store.limits);
                let malformed = EncodedSnapshot { stored, bytes };
                let result = store.put_exact(Some(1), &malformed);
                let unchanged =
                    load_settlement_snapshot(&store.root, &store.identity, &store.limits)
                        .unwrap()
                        .snapshots[0]
                        .bytes()
                        == first.bytes();
                if !matches!(
                    transition,
                    Err(SettlementStoreError::Invalid(
                        SettlementProblem::InvalidTransition
                    ))
                ) || !matches!(
                    decoded,
                    Err(SettlementStoreError::Invalid(SettlementProblem::Corrupt))
                ) || result != WriteResolution::Unresolved(SettlementProblem::Corrupt)
                    || store.health() != StoreHealth::Unhealthy(SettlementProblem::Corrupt)
                    || !unchanged
                {
                    violations.push(format!("{decision:?} axis={axis}: transition={transition:?}, decode_accepted={}, put={result:?}, unchanged={unchanged}", decoded.is_ok()));
                }
            }
        }
        assert!(
            violations.is_empty(),
            "partial rollback closure bypass:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn quality_q3_transition_decode_and_private_put_reject_partial_release() {
        use crate::{CostTuple, ReceiptId};
        let amount = |n| CostTuple {
            tokens_in: n,
            tokens_out: n,
            cents: n,
            wall_ms: n,
            attention_score: n,
        };
        let mut violations = Vec::new();
        for decision in [
            SettlementDecision::Refusal(RefusalClass::OutputBlocked),
            SettlementDecision::InfrastructureFailure(InfrastructureFailureClass::Storage),
            SettlementDecision::Completion { final_answer: true },
            SettlementDecision::Completion {
                final_answer: false,
            },
            SettlementDecision::Cancelled,
        ] {
            for axis in 0..6 {
                for first_put in [false, true] {
                    let (_temp, mut store, empty) = fixture();
                    let mut credit = amount(100);
                    match axis {
                        0 => credit.tokens_in = 20,
                        1 => credit.tokens_out = 20,
                        2 => credit.cents = 20,
                        3 => credit.wall_ms = 20,
                        4 => credit.attention_score = 20,
                        _ => credit = amount(20),
                    }
                    let app = DebitApplication {
                        epoch: empty.turn().budget_epoch,
                        requested_debit: CostTuple::ZERO,
                        applied_debit: amount(100).checked_sub(&credit).unwrap(),
                        reserved_credit: credit,
                        additional_debit: CostTuple::ZERO,
                        shortfall: CostTuple::ZERO,
                        rollback: RollbackStatus::None,
                    };
                    let mut t = empty.turn().clone();
                    t.rounds.push(RoundObligation {
                        settlement_id: SettlementId(Uuid::new_v4()),
                        ordinal: 0,
                        provider_request_id: Uuid::new_v4(),
                        provider: "fixture".into(),
                        model: "paid".into(),
                        request_digest: Sha256Digest::of(b"request"),
                        reserved: amount(100),
                        provider_evidence: ProviderEvidence::Observed {
                            usage: None,
                            cost: CostTuple::cents(12),
                            provenance: CostProvenance::ReportedCost,
                            finished: true,
                            interrupted: false,
                        },
                        tools: vec![],
                        known_incurred: CostTuple::cents(12),
                        decision: Some(decision.clone()),
                        phase: SettlementPhase::Unresolved {
                            last_definite: DefinitePhase::Finalized,
                            problem: SettlementProblem::ReleaseCreditPending,
                            application: Some(app.clone()),
                            candidate: None,
                        },
                        commit_ordinal: None,
                        projection: JournalProjection::NotConfigured,
                    });
                    t.terminal = TurnTerminal::Unresolved(SettlementProblem::ReleaseCreditPending);
                    let first = EncodedSnapshot::new(1, t.clone(), &store.limits).unwrap();
                    if !first_put {
                        assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
                    }
                    let receipt =
                        matches!(decision, SettlementDecision::Completion { .. }).then(|| {
                            ReceiptBinding {
                                receipt_id: ReceiptId::new(),
                                jws_digest: Sha256Digest::of(b"bound receipt"),
                            }
                        });
                    t.terminal = match &decision {
                        SettlementDecision::Completion { final_answer: true } => {
                            TurnTerminal::FinalAnswer(receipt.as_ref().unwrap().receipt_id)
                        }
                        SettlementDecision::Completion {
                            final_answer: false,
                        } => TurnTerminal::Open,
                        SettlementDecision::Cancelled => TurnTerminal::Cancelled,
                        SettlementDecision::Refusal(class) => TurnTerminal::Refused(*class),
                        SettlementDecision::InfrastructureFailure(class) => {
                            TurnTerminal::Failed(*class)
                        }
                    };
                    t.rounds[0].phase = SettlementPhase::Settled {
                        application: app,
                        receipt,
                    };
                    let transition = super::super::types::transition(
                        if first_put {
                            empty.turn()
                        } else {
                            first.turn()
                        },
                        &t,
                    );
                    // Bypass only construction: real envelope/digest, real put.
                    let stored = super::super::types::StoredSnapshot {
                        revision: if first_put { 1 } else { 2 },
                        payload_digest: Sha256Digest::of(&serde_json::to_vec(&t).unwrap()),
                        payload: t,
                    };
                    let bytes = serde_json::to_vec(&stored).unwrap();
                    let decoded = EncodedSnapshot::decode(bytes.clone(), &store.limits);
                    let malformed = EncodedSnapshot { stored, bytes };
                    let result =
                        store.put_exact(if first_put { None } else { Some(1) }, &malformed);
                    let inventory =
                        load_settlement_snapshot(&store.root, &store.identity, &store.limits)
                            .unwrap();
                    let unchanged = if first_put {
                        inventory.snapshots.is_empty()
                    } else {
                        inventory.snapshots[0].bytes() == first.bytes()
                    };
                    if !matches!(
                        transition,
                        Err(SettlementStoreError::Invalid(
                            SettlementProblem::InvalidTransition
                        ))
                    ) || !matches!(
                        decoded,
                        Err(SettlementStoreError::Invalid(SettlementProblem::Corrupt))
                    ) || result != WriteResolution::Unresolved(SettlementProblem::Corrupt)
                        || store.health() != StoreHealth::Unhealthy(SettlementProblem::Corrupt)
                        || !unchanged
                    {
                        violations.push(format!("{decision:?}/axis={axis}/first={first_put}: transition={transition:?}, decode={}, put={result:?}, unchanged={unchanged}", decoded.is_ok()));
                    }
                }
            }
        }
        assert!(
            violations.is_empty(),
            "partial release closure bypass:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn reopen_requires_successful_format_and_root_parent_sync() {
        for fault in [IoFault::OpenFormatSync, IoFault::OpenParentSync] {
            let (_temp, store, _) = fixture();
            let root = store.root.clone();
            let identity = store.identity.clone();
            let limits = store.limits;
            drop(store);
            assert!(
                matches!(
                    FileSettlementStore::open_inner(&root, identity.clone(), limits, fault),
                    Err(SettlementStoreError::Invalid(SettlementProblem::SyncFailed))
                ),
                "readable metadata is not a durability acknowledgement"
            );
            assert!(FileSettlementStore::open(&root, identity, limits).is_ok());
        }
    }

    #[test]
    fn failed_verification_or_sync_stays_unresolved_until_exact_resolution() {
        for fault in [
            IoFault::LostAckVerify,
            IoFault::LostAckFileSync,
            IoFault::LostAckParentSync,
        ] {
            let (_dir, mut store, snapshot) = fixture();
            store.fault = fault;
            let problem = if fault == IoFault::LostAckVerify {
                SettlementProblem::VerificationFailed
            } else {
                SettlementProblem::SyncFailed
            };
            assert_eq!(
                store.put_exact(None, &snapshot),
                WriteResolution::Unresolved(problem)
            );
            assert_eq!(store.health(), StoreHealth::Unhealthy(problem));
            assert_eq!(
                std::fs::read(
                    store
                        .root
                        .join(format!("{}.json", snapshot.turn().turn_id.0))
                )
                .unwrap(),
                snapshot.bytes(),
                "readability alone must not acknowledge"
            );
            store.fault = IoFault::None;
            let mut other = snapshot.turn().clone();
            other.turn_id = TurnId(Uuid::new_v4());
            let unrelated = EncodedSnapshot::new(1, other, &store.limits).unwrap();
            assert!(matches!(
                store.put_exact(None, &unrelated),
                WriteResolution::Unresolved(_)
            ));
            assert_eq!(
                store.resolve_exact(
                    snapshot.turn().turn_id,
                    snapshot.revision(),
                    Sha256Digest::of(b"wrong")
                ),
                WriteResolution::Unresolved(SettlementProblem::Conflict)
            );
            assert!(matches!(store.health(), StoreHealth::Unhealthy(_)));
            assert_eq!(
                store.resolve_exact(
                    snapshot.turn().turn_id,
                    snapshot.revision(),
                    snapshot.digest()
                ),
                WriteResolution::Durable(1)
            );
            assert_eq!(store.health(), StoreHealth::Healthy);
            assert_eq!(
                store.put_exact(None, &unrelated),
                WriteResolution::Durable(1)
            );
        }
    }

    #[test]
    fn rename_then_lost_ack_resolves_exact_bytes_once() {
        let (_dir, mut store, snapshot) = fixture();
        store.fault = IoFault::LostAck;
        assert_eq!(
            store.put_exact(None, &snapshot),
            WriteResolution::Durable(1)
        );
        assert_eq!(store.health(), StoreHealth::Healthy);
        let path = store
            .root
            .join(format!("{}.json", snapshot.turn().turn_id.0));
        let inode = std::fs::metadata(&path).unwrap().ino();
        assert_eq!(std::fs::read(&path).unwrap(), snapshot.bytes());
        assert_eq!(
            store.put_exact(None, &snapshot),
            WriteResolution::Durable(1)
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().ino(),
            inode,
            "duplicate does not rename again"
        );
        let root = store.root.clone();
        let identity = store.identity.clone();
        let limits = store.limits;
        drop(store);
        let inventory = load_settlement_snapshot(&root, &identity, &limits).unwrap();
        assert_eq!(inventory.snapshots.len(), 1);
        assert_eq!(inventory.snapshots[0].bytes(), snapshot.bytes());
    }

    #[test]
    fn definite_absence_allows_only_same_identity_bytes_to_retry() {
        for existing in [false, true] {
            let (_dir, mut store, first) = fixture();
            let next = if existing {
                assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
                EncodedSnapshot::new(2, first.turn().clone(), &store.limits).unwrap()
            } else {
                first.clone()
            };
            let previous = existing.then_some(1);
            store.fault = IoFault::BeforeWrite;
            assert_eq!(
                store.put_exact(previous, &next),
                WriteResolution::DefinitelyNotApplied
            );
            assert_eq!(
                store.resolve_exact(next.turn().turn_id, next.revision(), next.digest()),
                WriteResolution::DefinitelyNotApplied
            );
            store.fault = IoFault::None;
            assert_eq!(
                store.put_exact(previous, &next),
                WriteResolution::Durable(next.revision())
            );
            assert_eq!(store.health(), StoreHealth::Healthy);
        }
    }

    #[test]
    fn before_write_error_proves_absence_but_latches_unhealthy() {
        let (_dir, mut store, snapshot) = fixture();
        store.fault = IoFault::BeforeWrite;
        assert_eq!(
            store.put_exact(None, &snapshot),
            WriteResolution::DefinitelyNotApplied
        );
        assert_eq!(
            store.health(),
            StoreHealth::Unhealthy(SettlementProblem::Io)
        );
        assert!(
            !store
                .root
                .join(format!("{}.json", snapshot.turn().turn_id.0))
                .exists()
        );
    }
}
