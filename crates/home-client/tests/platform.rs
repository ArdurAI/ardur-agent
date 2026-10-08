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

#[tokio::test]
async fn invalid_unicode_pairing_fails_before_transport() {
    use home_client::{Error, pair_device};
    for code in [r#"{"homeName":"\ud800"}"#, r#"{"\udfff":1}"#] {
        let error = pair_device(code, "Test CLI").await.err().unwrap();
        assert_eq!(error, Error::InvalidUnicode);
        assert_eq!(error.code(), "invalid_unicode");
        assert_eq!(error.exit_code(), 3);
        assert_eq!(
            error.to_string(),
            "JSON strings must contain well-formed Unicode; unpaired surrogates are not allowed."
        );
    }
}
