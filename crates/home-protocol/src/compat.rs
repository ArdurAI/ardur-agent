//! The checked-in home contract used by the shipping client.
use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Compatibility {
    pub schema_version: u8,
    pub contract_revision: String,
    pub fixture_set_sha256: String,
    pub required_operations: Vec<String>,
}

pub static COMPATIBILITY: LazyLock<Compatibility> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../compat.json")).expect("checked compatibility manifest")
});

/// Resolve only trusted local operation names, never a name supplied by a home.
pub fn required_operation(name: &str) -> Option<&'static str> {
    COMPATIBILITY
        .required_operations
        .iter()
        .find(|op| *op == name)
        .map(String::as_str)
}
