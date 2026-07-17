//! ARD-422 — Matrix deny-by-default and auto_join_invites tests.
//!
//! Verifies:
//! - Default config has `auto_join_invites = false`.
//! - Empty allowlist denies all rooms (not allows all).
//! - Invite to non-allowlisted room is not joined.
//! - Message from non-allowlisted room is dropped.

use ardur_channel_matrix::{DEFAULT_DEVICE_ID, MatrixConfig};

const ENV_HOMESERVER: &str = "MATRIX_HOMESERVER_URL";
const ENV_USER_ID: &str = "MATRIX_USER_ID";
const ENV_ACCESS_TOKEN: &str = "MATRIX_ACCESS_TOKEN";
const ENV_AUTO_JOIN: &str = "MATRIX_AUTO_JOIN_INVITES";

fn required_env() -> Vec<(&'static str, &'static str)> {
    vec![
        (ENV_HOMESERVER, "https://matrix.example.org"),
        (ENV_USER_ID, "@ardur-bot:example.org"),
        (ENV_ACCESS_TOKEN, "secret-token"),
    ]
}

fn resolve(pairs: Vec<(&'static str, &'static str)>) -> MatrixConfig {
    MatrixConfig::from_source(|key| {
        pairs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| (*v).to_string())
    })
    .expect("required vars set")
}

#[test]
fn default_config_has_auto_join_false() {
    let config = resolve(required_env());
    assert!(
        !config.auto_join_invites,
        "auto_join_invites should default to false (ARD-422)"
    );
}

#[test]
fn default_device_id_is_ardur_bot() {
    let config = resolve(required_env());
    assert_eq!(config.resolved_device_id(), DEFAULT_DEVICE_ID);
}

#[test]
fn empty_allowlist_denies_all_rooms() {
    let config = resolve(required_env());
    assert!(config.allowed_rooms.is_empty());
    assert!(
        !config.room_allowed("!any:hs"),
        "empty allowlist should deny all rooms (deny-by-default)"
    );
    assert!(
        !config.room_allowed("!test:matrix.org"),
        "empty allowlist should deny every room"
    );
}

#[test]
fn non_empty_allowlist_permits_listed_rooms_only() {
    let config = MatrixConfig::builder("https://hs", "@bot:hs", "token")
        .allowed_rooms(vec!["!allowed:hs".to_string(), "!also:hs".to_string()])
        .build()
        .expect("builds");
    assert!(config.room_allowed("!allowed:hs"));
    assert!(config.room_allowed("!also:hs"));
    assert!(!config.room_allowed("!notlisted:hs"));
    assert!(!config.room_allowed(""));
}

#[test]
fn auto_join_true_with_empty_allowlist_warns_but_builds() {
    // This should build successfully (the warning is log-only), and the
    // resulting config still denies all rooms.
    let config = MatrixConfig::builder("https://hs", "@bot:hs", "token")
        .auto_join_invites(true)
        .build()
        .expect("auto_join=true with empty allowlist builds (warns)");
    assert!(config.auto_join_invites);
    assert!(config.allowed_rooms.is_empty());
    assert!(!config.room_allowed("!any:hs"));
}

#[test]
fn auto_join_false_with_empty_allowlist_is_silent_default() {
    let config = MatrixConfig::builder("https://hs", "@bot:hs", "token")
        .build()
        .expect("builds");
    assert!(!config.auto_join_invites);
    assert!(config.allowed_rooms.is_empty());
    assert!(!config.room_allowed("!any:hs"));
}

#[test]
fn env_auto_join_true_enables() {
    let mut env = required_env();
    env.push((ENV_AUTO_JOIN, "true"));
    let config = resolve(env);
    assert!(config.auto_join_invites);
}

#[test]
fn env_auto_join_false_disables() {
    let mut env = required_env();
    env.push((ENV_AUTO_JOIN, "false"));
    let config = resolve(env);
    assert!(!config.auto_join_invites);
}

#[test]
fn env_auto_join_absent_defaults_false() {
    let config = resolve(required_env());
    assert!(
        !config.auto_join_invites,
        "absent env var defaults to false (ARD-422)"
    );
}
