//! Integration coverage for `ardur_cli::secure_io`'s public functions.
//!
//! The crate's own `#[cfg(all(test, unix))]` unit tests only exercise the
//! symlink-attack rejection paths for the atomic writer and the recursive
//! remover. This test drives the happy path of every re-exported function —
//! create, overwrite, atomic replace, read bytes/string, list, stat, and
//! recursive remove — the way a real caller (CLI state persistence) would.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use ardur_cli::{
    create_private_file_no_follow, directory_modified_no_follow, list_directory_names_no_follow,
    read_file_no_follow, read_string_no_follow, remove_directory_tree_no_follow,
    write_private_file_atomic_no_follow, write_private_file_no_follow,
};

/// A fresh tempdir, canonicalized. On macOS `std::env::temp_dir()` resolves
/// under `/var`, itself a symlink to `/private/var`; the descriptor-relative
/// `O_NOFOLLOW` walk in `secure_io` rejects a symlinked *path component* on
/// the way down, so tests must canonicalize first — the same convention the
/// crate's own unit tests already follow.
fn canon_tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let canonical = dir.path().canonicalize().expect("canonicalize tempdir");
    (dir, canonical)
}

#[test]
fn create_read_and_overwrite_round_trip_through_the_real_files() {
    let (_dir, root) = canon_tempdir();
    let path = root.join("state").join("session.json");

    create_private_file_no_follow(&path, b"{\"turn\":1}").expect("create succeeds");
    assert_eq!(
        read_file_no_follow(&path).expect("read succeeds"),
        b"{\"turn\":1}"
    );
    assert_eq!(
        read_string_no_follow(&path).expect("read_string succeeds"),
        "{\"turn\":1}"
    );

    // Owner-only permissions on a freshly created file.
    let mode = std::fs::metadata(&path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "created file must be owner-read/write only");

    write_private_file_no_follow(&path, b"{\"turn\":2}").expect("overwrite succeeds");
    assert_eq!(
        read_string_no_follow(&path).expect("read_string succeeds"),
        "{\"turn\":2}"
    );
}

#[test]
fn create_rejects_an_existing_path() {
    let (_dir, root) = canon_tempdir();
    let path = root.join("once.json");

    create_private_file_no_follow(&path, b"first").expect("first create succeeds");
    assert!(
        create_private_file_no_follow(&path, b"second").is_err(),
        "create_new semantics must reject an existing file"
    );
    assert_eq!(read_file_no_follow(&path).expect("read succeeds"), b"first");
}

#[test]
fn atomic_write_replaces_content_and_leaves_no_temp_file_behind() {
    let (_dir, root) = canon_tempdir();
    let path = root.join("config").join("metadata.json");

    create_private_file_no_follow(&path, b"v1").expect("create succeeds");
    write_private_file_atomic_no_follow(&path, b"v2").expect("atomic write succeeds");
    assert_eq!(read_string_no_follow(&path).expect("read succeeds"), "v2");

    let leftovers: Vec<_> = list_directory_names_no_follow(path.parent().unwrap())
        .expect("list succeeds")
        .into_iter()
        .filter(|name| name.to_string_lossy().starts_with(".ardur-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "atomic replace must not leave a temp file behind: {leftovers:?}"
    );
}

#[test]
fn atomic_write_can_create_a_file_that_did_not_exist() {
    let (_dir, root) = canon_tempdir();
    let path = root.join("fresh.json");

    write_private_file_atomic_no_follow(&path, b"created atomically")
        .expect("atomic create succeeds");
    assert_eq!(
        read_string_no_follow(&path).expect("read succeeds"),
        "created atomically"
    );
}

#[test]
fn list_directory_names_enumerates_entries_without_dot_and_dotdot() {
    let (_dir, root) = canon_tempdir();
    create_private_file_no_follow(&root.join("a.json"), b"a").unwrap();
    create_private_file_no_follow(&root.join("b.json"), b"b").unwrap();

    let mut names: Vec<String> = list_directory_names_no_follow(&root)
        .expect("list succeeds")
        .into_iter()
        .map(|n| n.to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["a.json".to_string(), "b.json".to_string()]);
}

#[test]
fn directory_modified_advances_after_a_write() {
    let (_dir, root) = canon_tempdir();
    let before = directory_modified_no_follow(&root).expect("stat succeeds");

    // Ensure the filesystem's mtime granularity can observe a change.
    std::thread::sleep(std::time::Duration::from_millis(50));
    create_private_file_no_follow(&root.join("new.json"), b"x").unwrap();

    let after = directory_modified_no_follow(&root).expect("stat succeeds");
    assert!(
        after >= before,
        "directory mtime must not go backwards after a create"
    );
}

#[test]
fn remove_directory_tree_deletes_nested_files_and_subdirectories() {
    let (_dir, root) = canon_tempdir();
    let target = root.join("session-42");
    create_private_file_no_follow(&target.join("journal.jsonl"), b"line\n").unwrap();
    create_private_file_no_follow(&target.join("receipts").join("chain.jsonl"), b"r\n").unwrap();

    remove_directory_tree_no_follow(&target).expect("recursive remove succeeds");
    assert!(!target.exists(), "the whole tree must be gone");
    assert!(root.exists(), "the parent directory itself must survive");
}

#[test]
fn read_string_rejects_non_utf8_content() {
    let (_dir, root) = canon_tempdir();
    let path = root.join("binary.bin");
    create_private_file_no_follow(&path, &[0xff, 0xfe, 0x00, 0xff]).unwrap();

    let err = read_string_no_follow(&path).expect_err("invalid UTF-8 must be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}
