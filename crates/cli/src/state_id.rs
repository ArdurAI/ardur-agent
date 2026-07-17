//! Validates that a user-supplied id/name is safe to use as a single path
//! component under a `~/.ardur/...` state directory.
//!
//! Several CLI subsystems (personas, marketplace records, approvals,
//! schedules, tokens, channels, memory tombstones, ...) map a caller-supplied
//! id straight onto `<state_dir>.join(format!("{id}.json"))`. `Path::join`
//! replaces its base entirely when the joined component is absolute, and a
//! `..` component walks back out of it, so an unsanitized id is a
//! path-traversal / arbitrary-file read-write-delete primitive, not just a
//! cosmetic input-validation gap. Every such call site must run the id
//! through [`sanitize_state_id`] first.

use ardur_cli::CliError;

/// Reject an id/name that would escape its intended state directory when
/// joined as `<dir>.join(format!("{id}...")`.
///
/// Denies: empty strings, `.` and `..`, any path separator (`/` or `\`,
/// covering both Unix and Windows joins), a NUL byte, and any id that
/// `Path::is_absolute` considers rooted (covers absolute Unix paths and
/// Windows drive/UNC forms). Anything else — including dots, colons, and
/// other punctuation that legitimate ids (reverse-DNS style package ids,
/// UUIDs, timestamps) commonly contain — is accepted; this is a traversal
/// guard, not a charset allowlist.
pub(crate) fn sanitize_state_id(id: &str) -> Result<(), CliError> {
    if id.is_empty() {
        return Err(CliError::State("id must not be empty".to_string()));
    }
    if id == "." || id == ".." {
        return Err(CliError::State(format!("id `{id}` is not allowed")));
    }
    if id.contains('/') || id.contains('\\') || id.contains('\0') {
        return Err(CliError::State(format!(
            "id `{id}` must not contain a path separator"
        )));
    }
    if std::path::Path::new(id).is_absolute() {
        return Err(CliError::State(format!(
            "id `{id}` must not be an absolute path"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_ids() {
        for ok in [
            "my-persona",
            "com.example.tool",
            "018f4d1e-6b3a-7c9e-9d2a-000000000000",
            "token_2026-07-12T00:00:00Z",
        ] {
            assert!(sanitize_state_id(ok).is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        for bad in [
            "",
            ".",
            "..",
            "../../../etc/passwd",
            "..\\..\\windows\\system32",
            "/etc/passwd",
            "/home/victim/.ssh/id_rsa",
            "sub/dir",
            "a\0b",
        ] {
            assert!(sanitize_state_id(bad).is_err(), "should reject {bad:?}");
        }
    }
}
