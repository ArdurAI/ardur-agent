#[cfg(not(unix))]
#[test]
fn private_file_backend_refuses_unprotected_platforms() {
    use home_client::{FileStore, SecretStore};
    let store = FileStore::new(std::path::PathBuf::from("unused-private-state"));
    assert!(store.prepare().is_err());
    assert!(store.load().is_err());
}
#[test]
fn human_text_cannot_emit_terminal_controls() {
    let text = home_client::human_text("Home\u{1b}[2J\n\u{9b}\u{202e}");
    assert!(!text.chars().any(char::is_control));
    assert!(!text.contains('\u{202e}'));
    assert!(text.contains("\\u{1b}"));
}
