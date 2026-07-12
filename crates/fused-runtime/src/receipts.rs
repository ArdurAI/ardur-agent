//! Durable receipt-chain persistence and cross-restart linkage verification.
//!
//! The fused runtime appends each turn's signed receipt to an append-only log,
//! one compact JWS per line (the receipt crate names the JWS "the canonical,
//! hashed-over form used for chaining"). A fresh runtime over the same path
//! resumes the chain from the last line's hash, and
//! [`verify_persisted_chain`] re-checks the whole chain off disk.
//!
//! Why we re-implement the linkage check instead of calling
//! [`ardur_receipt::verify_chain`]: that function takes
//! `&[SignedReceipt]`, and a `SignedReceipt` cannot be reconstructed outside the
//! receipt crate (its `from_parts` constructor is crate-private). So we persist
//! the JWS, decode each one's body for its `parent_hash`, and apply the *same*
//! rule `verify_chain` does — `parent_hash[i] == SHA256(jws[i-1])`, genesis
//! carries `None`.

use std::io::Read as _;
use std::path::Path;

use ardur_receipt::{Jwks, ReceiptBody, ReceiptError, ReceiptVerifier, Sha256Digest};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;

/// One receipt as persisted on disk: its canonical compact JWS plus the
/// [`ReceiptBody`] decoded from that JWS's payload segment.
#[derive(Clone, Debug)]
pub struct PersistedReceipt {
    /// The compact JWS string (`header.payload.sig`) — the bytes a child
    /// receipt's `parent_hash` is the SHA-256 of.
    pub jws_compact: String,
    /// The body decoded from the JWS payload segment.
    pub body: ReceiptBody,
}

/// A failure loading or verifying a persisted receipt chain.
#[derive(Debug, thiserror::Error)]
pub enum ReceiptChainError {
    /// The receipt-log file could not be read.
    #[error("receipt log i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// A line was not a well-formed compact JWS, or its payload did not decode
    /// to a [`ReceiptBody`].
    #[error("malformed persisted receipt: {0}")]
    Malformed(String),
    /// The hash linkage broke at receipt index `at`: its `parent_hash` did not
    /// equal the SHA-256 of the previous receipt's JWS (or the genesis receipt
    /// carried a non-`None` parent).
    #[error("broken receipt chain at index {at}")]
    BrokenChain {
        /// Index into the loaded chain at which the mismatch was found.
        at: usize,
    },
    /// A compact JWS failed ES256 verification against the supplied JWKS.
    #[error("invalid receipt signature at index {at}: {source}")]
    InvalidSignature {
        /// Index into the loaded chain at which verification failed.
        at: usize,
        /// Signature or protected-header verification failure.
        #[source]
        source: ReceiptError,
    },
    /// A caller supplied a decoded body that does not match the authenticated
    /// compact JWS payload.
    #[error("persisted receipt body mismatch at index {at}")]
    BodyMismatch {
        /// Index into the loaded chain at which the mismatch was found.
        at: usize,
    },
}

/// Decode the [`ReceiptBody`] out of a compact JWS's payload (middle) segment.
fn decode_body(jws_compact: &str) -> Result<ReceiptBody, ReceiptChainError> {
    let payload_b64 = jws_compact
        .split('.')
        .nth(1)
        .ok_or_else(|| ReceiptChainError::Malformed("not three JWS segments".to_string()))?;
    let payload = B64URL
        .decode(payload_b64)
        .map_err(|e| ReceiptChainError::Malformed(format!("payload base64url: {e}")))?;
    serde_json::from_slice(&payload)
        .map_err(|e| ReceiptChainError::Malformed(format!("payload json: {e}")))
}

fn require_regular(file: &std::fs::File, path: &Path) -> std::io::Result<()> {
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("receipt log is not a regular file: {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn errno_to_io(error: rustix::io::Errno) -> std::io::Error {
    if error == rustix::io::Errno::LOOP || error == rustix::io::Errno::NOTDIR {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing symlink or non-directory in receipt-log path",
        )
    } else {
        std::io::Error::from_raw_os_error(error.raw_os_error())
    }
}

#[cfg(unix)]
fn open_parent_directory_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, openat};

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "receipt log has no parent",
        )
    })?;
    let trusted_root = parent.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "receipt log parent has no trusted root",
        )
    })?;
    let parent_name = parent.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "receipt log parent has no file name",
        )
    })?;
    let trusted = {
        use rustix::fs::{CWD, Mode, OFlags, openat};
        let fd = openat(
            CWD,
            trusted_root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(errno_to_io)?;
        std::fs::File::from(fd)
    };
    let descriptor = openat(
        &trusted,
        parent_name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(errno_to_io)?;
    Ok(std::fs::File::from(descriptor))
}

fn open_regular_no_follow(path: &Path, write: bool) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, openat};

        let parent = open_parent_directory_no_follow(path)?;
        let name = path.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "receipt log has no file name",
            )
        })?;
        let access = if write { OFlags::RDWR } else { OFlags::RDONLY };
        let descriptor = openat(
            &parent,
            name,
            access | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(errno_to_io)?;
        std::fs::File::from(descriptor)
    };
    #[cfg(not(unix))]
    let file = {
        let mut options = std::fs::OpenOptions::new();
        options.read(!write).write(write);
        options.open(path)?
    };
    require_regular(&file, path)?;
    Ok(file)
}

pub(crate) fn open_append_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, openat};

        let parent = open_parent_directory_no_follow(path)?;
        let name = path.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "receipt log has no file name",
            )
        })?;
        let descriptor = openat(
            &parent,
            name,
            OFlags::WRONLY | OFlags::APPEND | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(errno_to_io)?;
        std::fs::File::from(descriptor)
    };
    #[cfg(not(unix))]
    let file = {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        options.open(path)?
    };
    require_regular(&file, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Atomically replace a receipt log from an anchored parent descriptor without
/// following either a stale temporary-file symlink or a substituted parent.
pub(crate) fn replace_receipt_log_no_follow(path: &Path, body: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, openat, renameat};
        use std::io::Write as _;

        let parent = open_parent_directory_no_follow(path)?;
        let name = path.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "receipt log has no file name",
            )
        })?;
        let tmp_name = format!(
            ".{}.{}.reconcile-tmp",
            name.to_string_lossy(),
            uuid::Uuid::new_v4().simple()
        );
        let descriptor = openat(
            &parent,
            tmp_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(errno_to_io)?;
        let mut tmp = std::fs::File::from(descriptor);
        require_regular(&tmp, Path::new(&tmp_name))?;
        tmp.write_all(body)?;
        tmp.flush()?;
        tmp.sync_all()?;
        if let Err(e) = renameat(&parent, &tmp_name, &parent, name).map_err(errno_to_io) {
            let _ = rustix::fs::unlinkat(&parent, &tmp_name, rustix::fs::AtFlags::empty());
            return Err(e);
        }
        parent.sync_all()?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        use std::io::Write as _;

        let tmp = path.with_extension("jsonl.reconcile-tmp");
        for candidate in [path, tmp.as_path()] {
            if let Ok(metadata) = std::fs::symlink_metadata(candidate) {
                if metadata.file_type().is_symlink() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("refusing symlink receipt path: {}", candidate.display()),
                    ));
                }
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        require_regular(&file, &tmp)?;
        file.write_all(body)?;
        file.flush()?;
        file.sync_all()?;
        std::fs::rename(tmp, path)
    }
}

/// Load every persisted receipt from `path`, in append (chain) order. A missing
/// file is an empty chain (no turns have been receipted yet). A single malformed
/// unterminated tail is treated as a torn write from a crash: it is dropped and
/// the file is truncated back to the last complete line so future appends cannot
/// concatenate onto corrupt bytes.
pub fn load_persisted_chain(
    path: impl AsRef<Path>,
) -> Result<Vec<PersistedReceipt>, ReceiptChainError> {
    let path = path.as_ref();
    let mut file = match open_regular_no_follow(path, false) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ReceiptChainError::Io(e)),
    };
    let mut raw = String::new();
    file.read_to_string(&mut raw)?;
    let (chain, torn_tail_start) = parse_receipt_lines(&raw)?;
    if let Some(offset) = torn_tail_start {
        tracing::warn!(
            path = %path.display(),
            truncate_at = offset,
            "dropping torn trailing receipt-log line"
        );
        let file = open_regular_no_follow(path, true)?;
        file.set_len(offset as u64)?;
        file.sync_all()?;
    }
    Ok(chain)
}

fn parse_receipt_lines(
    raw: &str,
) -> Result<(Vec<PersistedReceipt>, Option<usize>), ReceiptChainError> {
    let mut chain = Vec::new();
    let mut offset = 0;
    for (line_index, segment) in raw.split_inclusive('\n').enumerate() {
        let segment_start = offset;
        offset += segment.len();
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        if line.trim().is_empty() {
            continue;
        }
        match decode_body(line) {
            Ok(body) => chain.push(PersistedReceipt {
                jws_compact: line.to_string(),
                body,
            }),
            Err(err) if !segment.ends_with('\n') && offset == raw.len() => {
                tracing::warn!(
                    line_index,
                    error = %err,
                    "ignoring malformed trailing receipt-log line"
                );
                return Ok((chain, Some(segment_start)));
            }
            Err(err) => return Err(err),
        }
    }
    Ok((chain, None))
}

/// Verify the hash linkage of a loaded chain: the first receipt must be a
/// genesis (`parent_hash == None`) and every later receipt's `parent_hash` must
/// equal `SHA256` of the previous receipt's JWS. Returns the index of the first
/// break, mirroring [`ardur_receipt::verify_chain`]'s rule over on-disk data.
pub fn verify_persisted_chain(chain: &[PersistedReceipt]) -> Result<(), ReceiptChainError> {
    let mut prev: Option<&PersistedReceipt> = None;
    for (at, receipt) in chain.iter().enumerate() {
        let expected = prev.map(|p| Sha256Digest::of(p.jws_compact.as_bytes()));
        if receipt.body.parent_hash != expected {
            return Err(ReceiptChainError::BrokenChain { at });
        }
        prev = Some(receipt);
    }
    Ok(())
}

/// Authenticate every compact JWS against `jwks` and verify hash linkage using
/// the authenticated payloads. This is the fail-closed verifier for persisted
/// receipt evidence; [`verify_persisted_chain`] alone proves ordering, not signer
/// identity.
pub fn verify_persisted_chain_with_jwks(
    chain: &[PersistedReceipt],
    jwks: &Jwks,
) -> Result<(), ReceiptChainError> {
    let mut previous_jws: Option<&str> = None;
    for (at, receipt) in chain.iter().enumerate() {
        let verified = ReceiptVerifier::verify_compact(&receipt.jws_compact, jwks)
            .map_err(|source| ReceiptChainError::InvalidSignature { at, source })?;
        if verified.body != receipt.body {
            return Err(ReceiptChainError::BodyMismatch { at });
        }
        let expected = previous_jws.map(|jws| Sha256Digest::of(jws.as_bytes()));
        if verified.body.parent_hash != expected {
            return Err(ReceiptChainError::BrokenChain { at });
        }
        previous_jws = Some(&receipt.jws_compact);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ardur_receipt::{
        CostTuple, Es256SigningKey, HolderId, Jwks, ReceiptSigner, TokenId, UnixTsMillis,
        VerbObject,
    };

    use super::*;

    fn sample_body() -> ReceiptBody {
        ReceiptBody {
            receipt_id: uuid::Uuid::new_v4(),
            parent_hash: None,
            verb: VerbObject::new("cost.admission.allow.v1").expect("valid verb"),
            issued_at: UnixTsMillis(1_700_000_000_000),
            subject: HolderId("spiffe://ardur/test".to_string()),
            cap_token_id: TokenId("test-token".to_string()),
            payload_digest: Sha256Digest::of(b"payload"),
            session_id: None,
            cost: CostTuple {
                tokens_in: 0,
                tokens_out: 0,
                cents: 0,
                wall_ms: 0,
                attention_score: 0,
            },
            tool_calls: Vec::new(),
            provider: Some("test-provider".to_string()),
        }
    }

    #[cfg(unix)]
    #[test]
    fn receipt_log_loader_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target.jsonl");
        let link = dir.path().join("chain.jsonl");
        std::fs::write(&target, "").expect("target receipt log");
        symlink(&target, &link).expect("receipt log symlink");

        let error =
            load_persisted_chain(&link).expect_err("symlinked receipt log must fail closed");
        assert!(error.to_string().contains("symlink"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn receipt_log_rejects_symlinked_parent_directory() {
        use std::os::unix::fs::symlink;

        let trusted = tempfile::tempdir().expect("trusted root");
        let outside = tempfile::tempdir().expect("outside root");
        let parent = trusted.path().join("receipts");
        symlink(outside.path(), &parent).expect("symlink receipt parent");
        let log = parent.join("chain.jsonl");

        let read_error = load_persisted_chain(&log)
            .expect_err("symlinked receipt parent must fail closed on read");
        assert!(read_error.to_string().contains("symlink"), "{read_error}");
        let append_error = open_append_no_follow(&log)
            .expect_err("symlinked receipt parent must fail closed on append");
        assert!(
            append_error.to_string().contains("symlink"),
            "{append_error}"
        );
        assert!(!outside.path().join("chain.jsonl").exists());
    }

    #[test]
    fn persisted_chain_requires_valid_es256_signatures() {
        let key = Es256SigningKey::generate();
        let jwks = Jwks::from_public_key(&key.public_key());
        let signed = ReceiptSigner::sign(sample_body(), &key).expect("sign receipt");
        let mut persisted = PersistedReceipt {
            jws_compact: signed.jws_compact().to_string(),
            body: signed.body().clone(),
        };

        verify_persisted_chain_with_jwks(std::slice::from_ref(&persisted), &jwks)
            .expect("valid persisted receipt verifies");

        let signature_start = persisted
            .jws_compact
            .rfind('.')
            .expect("compact JWS signature segment")
            + 1;
        let first_signature_char = persisted.jws_compact.as_bytes()[signature_start] as char;
        persisted.jws_compact.replace_range(
            signature_start..=signature_start,
            if first_signature_char == 'A' {
                "B"
            } else {
                "A"
            },
        );
        assert!(
            verify_persisted_chain_with_jwks(&[persisted], &jwks).is_err(),
            "hash-link validity alone must not authenticate a forged signature"
        );
    }
}
