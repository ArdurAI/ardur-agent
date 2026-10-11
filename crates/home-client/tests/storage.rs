#![cfg(unix)]
use home_client::{FileStore, Profile, SecretStore, StoredHome};
use home_protocol::{DeviceKeys, HomePins};
use std::os::unix::fs::{PermissionsExt, symlink};
fn home() -> StoredHome {
    StoredHome {
        profile: Profile {
            schema_version: 1,
            url: "https://home.test".into(),
            home_name: "Home".into(),
            pins: HomePins {
                instance_id: "home".into(),
                fingerprint: "a".repeat(64),
                certificate_fingerprint: "b".repeat(64),
            },
            grant_id: "grant".into(),
            space_id: "space".into(),
        },
        private_key: DeviceKeys::generate().unwrap().private_key,
    }
}
#[test]
fn private_roundtrip_no_challenge_and_unsafe_permissions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("ardur");
    let store = FileStore::new(root.clone());
    store.prepare().unwrap();
    store.save(&home()).unwrap();
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let path = root.join("paired-home.json");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("challenge"));
    assert_eq!(store.load().unwrap().profile.grant_id, "grant");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.load().is_err());
    assert!(store.save(&home()).is_err());
}
#[test]
fn refuse_symlink_hardlink_and_broad_directory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("ardur");
    let store = FileStore::new(root.clone());
    store.prepare().unwrap();
    store.save(&home()).unwrap();
    let path = root.join("paired-home.json");
    std::fs::hard_link(&path, root.join("alias")).unwrap();
    assert!(store.load().is_err());
    std::fs::remove_file(root.join("alias")).unwrap();
    std::fs::rename(&path, root.join("real")).unwrap();
    symlink("real", &path).unwrap();
    assert!(store.load().is_err());
    assert!(store.save(&home()).is_err());
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(store.prepare().is_err());
}
#[test]
fn invalid_key_and_oversize_are_safe_errors() {
    let temp = tempfile::tempdir().unwrap();
    let store = FileStore::new(temp.path().canonicalize().unwrap().join("ardur"));
    store.prepare().unwrap();
    let mut h = home();
    h.private_key = zeroize::Zeroizing::new("sensitive-canary".into());
    assert!(store.save(&h).is_err());
    store.save(&home()).unwrap();
    let path = store.path().join("paired-home.json");
    std::fs::write(path, vec![b'x'; 65537]).unwrap();
    let e = store.load().err().unwrap();
    assert!(!e.to_string().contains("sensitive-canary"));
}

#[test]
fn bounded_serialization_preserves_existing_state() {
    let temp = tempfile::tempdir().unwrap();
    let store = FileStore::new(temp.path().canonicalize().unwrap().join("ardur"));
    store.save(&home()).unwrap();
    let path = store.path().join("paired-home.json");
    let before = std::fs::read(&path).unwrap();
    let mut oversized = home();
    oversized.profile.url = format!("https://{}.test", "a".repeat(65536));
    oversized.validate().unwrap();
    assert!(store.save(&oversized).is_err());
    assert!(std::fs::read(&path).unwrap() == before);
    assert_eq!(store.load().unwrap().profile.grant_id, "grant");
    oversized.private_key = zeroize::Zeroizing::new("x".repeat(4097));
    assert!(store.save(&oversized).is_err());
    assert!(std::fs::read(&path).unwrap() == before);
}

#[test]
fn version_one_profile_still_loads_without_rewriting_private_state() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("ardur");
    let store = FileStore::new(root.clone());
    store.prepare().unwrap();
    let h = home();
    // The original format has always been schemaVersion 1 with a byte-array key.
    // Construct it independently of the current writer to catch format drift.
    let old = serde_json::json!({"profile":{
        "schemaVersion":1,"url":"https://home.test","homeName":"Home",
        "pins":{"instanceId":"home","fingerprint":"a".repeat(64),"certificateFingerprint":"b".repeat(64)},
        "grantId":"grant","spaceId":"space"
    },"privateKey":h.private_key.as_bytes()});
    let path = root.join("paired-home.json");
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.write_all(old.to_string().as_bytes()).unwrap();
    let before = std::fs::read(&path).unwrap();
    let loaded = store.load().unwrap();
    assert_eq!(loaded.profile.schema_version, 1);
    assert_eq!(loaded.private_key, h.private_key);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let mut unsupported = old.clone();
    unsupported["profile"]["schemaVersion"] = serde_json::json!(2);
    std::fs::write(&path, unsupported.to_string()).unwrap();
    assert_eq!(store.load().err().unwrap(), home_client::Error::Storage);
    unsupported["profile"]
        .as_object_mut()
        .unwrap()
        .remove("schemaVersion");
    std::fs::write(&path, unsupported.to_string()).unwrap();
    assert_eq!(store.load().err().unwrap(), home_client::Error::Storage);
}
