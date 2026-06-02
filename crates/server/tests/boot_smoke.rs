//! `boot_smoke` — [`AppState::boot`] wires the whole substrate over a stub
//! provider + tempdir without panicking, and lays down the persistent state
//! directory layout (the keys it mints on first boot, and the data subdirs).

mod support;

#[tokio::test]
async fn boots_and_lays_down_the_state_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);

    let state = support::boot_stub(&config);
    assert_eq!(state.data_dir(), dir.path());

    // The four state subdirectories exist.
    for sub in ["memory", "journals", "receipts", "keys"] {
        assert!(
            dir.path().join(sub).is_dir(),
            "boot creates the {sub}/ subdirectory"
        );
    }

    // The two long-lived keys were minted + persisted on first boot.
    assert!(
        dir.path().join("keys/issuer.key").is_file(),
        "the cap-token issuer key is persisted"
    );
    assert!(
        dir.path().join("keys/receipt.pem").is_file(),
        "the receipt signing key is persisted"
    );
}

/// A second boot over the *same* data dir reuses the persisted keys (it does not
/// panic re-reading them) — the restart path.
#[tokio::test]
async fn second_boot_reuses_persisted_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);

    let _first = support::boot_stub(&config);
    let issuer_key = std::fs::read_to_string(dir.path().join("keys/issuer.key")).expect("read key");

    let _second = support::boot_stub(&config);
    let reread = std::fs::read_to_string(dir.path().join("keys/issuer.key")).expect("read key");

    assert_eq!(issuer_key, reread, "the issuer key is stable across boots");
}
