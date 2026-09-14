//! Parsing `[integrations.<name>]` out of a TOML document.
//!
//! The parse is deliberately strict. Configuration that cannot be given a
//! precise meaning is rejected rather than approximated, because the failure
//! mode of a permissive parse here is an operator who believes an integration
//! is confined when it is not.

use std::collections::BTreeSet;
use std::path::PathBuf;

use toml::Value;

use crate::{Integration, IntegrationEndpoint, IntegrationName, IntegrationSet, NameError};

/// Keys permitted inside an `[integrations.<name>]` block.
///
/// Enumerated so that an unknown key is an error. A silently-ignored
/// `enable = true` (for `enabled`) would leave an operator convinced they had
/// switched something on.
const KNOWN_KEYS: &[&str] = &["enabled", "command", "root", "capabilities"];

/// Why an integrations block was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The document was not valid TOML.
    #[error("configuration is not valid TOML: {message}")]
    Toml {
        /// The underlying parser's message.
        message: String,
    },
    /// `integrations` was present but was not a table.
    #[error("`integrations` must be a table of named integrations, found {found}")]
    IntegrationsNotATable {
        /// The TOML type actually found.
        found: &'static str,
    },
    /// `[integrations.<name>]` was not a table.
    #[error("`integrations.{name}` must be a table, found {found}")]
    EntryNotATable {
        /// The integration name.
        name: String,
        /// The TOML type actually found.
        found: &'static str,
    },
    /// The name violated the documented alphabet.
    #[error("`integrations.{name}` is not a usable name: {source}")]
    Name {
        /// The offending name.
        name: String,
        /// Why it was refused.
        #[source]
        source: NameError,
    },
    /// A key inside the block is not one this binary understands.
    #[error(
        "`integrations.{name}` has unknown key `{key}` (expected one of: {expected}); \
         refusing rather than ignoring it, because an ignored key looks like a \
         setting that took effect"
    )]
    UnknownKey {
        /// The integration name.
        name: String,
        /// The offending key.
        key: String,
        /// The permitted keys.
        expected: String,
    },
    /// A key held the wrong TOML type.
    #[error("`integrations.{name}.{key}` must be {expected}, found {found}")]
    WrongType {
        /// The integration name.
        name: String,
        /// The offending key.
        key: &'static str,
        /// What was required.
        expected: &'static str,
        /// What was found.
        found: &'static str,
    },
    /// Neither `command` nor `root` was given.
    #[error(
        "`integrations.{name}` declares no endpoint: exactly one of `command` \
         (an executable driven through argv-exec) or `root` (a confining \
         directory) is required"
    )]
    MissingEndpoint {
        /// The integration name.
        name: String,
    },
    /// Both `command` and `root` were given.
    #[error(
        "`integrations.{name}` declares both `command` and `root`, but an \
         integration has exactly one endpoint; the confinement rules for an \
         executable and a directory are different and cannot both apply"
    )]
    AmbiguousEndpoint {
        /// The integration name.
        name: String,
    },
    /// An endpoint or capability was an empty string.
    #[error("`integrations.{name}.{key}` cannot be empty")]
    EmptyValue {
        /// The integration name.
        name: String,
        /// The offending key.
        key: &'static str,
    },
}

/// The TOML type name of a value, for error messages.
fn type_of(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "a string",
        Value::Integer(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Boolean(_) => "a boolean",
        Value::Datetime(_) => "a datetime",
        Value::Array(_) => "an array",
        Value::Table(_) => "a table",
    }
}

/// Parse every `[integrations.<name>]` block in `document`.
///
/// A document with no `integrations` table is not an error — it is the default
/// posture, and yields an empty set.
///
/// # Errors
///
/// Returns [`ParseError`] if the document is not valid TOML, or if any
/// integration block is malformed. Validation is strict: unknown keys, wrong
/// types, and missing or ambiguous endpoints all fail the load rather than
/// being silently normalised.
pub fn parse_integrations(document: &str) -> Result<IntegrationSet, ParseError> {
    let root: Value = toml::from_str(document).map_err(|e| ParseError::Toml {
        message: e.to_string(),
    })?;

    let Some(table) = root.get("integrations") else {
        // No `integrations` key at all: the overwhelmingly common case, and
        // the one that must stay cheap and silent.
        return Ok(IntegrationSet::new());
    };

    let table = table
        .as_table()
        .ok_or_else(|| ParseError::IntegrationsNotATable {
            found: type_of(table),
        })?;

    let mut set = IntegrationSet::new();
    for (raw_name, entry) in table {
        set.insert(parse_entry(raw_name, entry)?);
    }
    Ok(set)
}

/// Parse a single `[integrations.<name>]` block.
fn parse_entry(raw_name: &str, entry: &Value) -> Result<Integration, ParseError> {
    let name = IntegrationName::new(raw_name).map_err(|source| ParseError::Name {
        name: raw_name.to_string(),
        source,
    })?;

    let table = entry.as_table().ok_or_else(|| ParseError::EntryNotATable {
        name: raw_name.to_string(),
        found: type_of(entry),
    })?;

    // Reject unknown keys before reading known ones, so an operator sees the
    // typo rather than a downstream consequence of it.
    let known: BTreeSet<&str> = KNOWN_KEYS.iter().copied().collect();
    if let Some(unknown) = table.keys().find(|k| !known.contains(k.as_str())) {
        return Err(ParseError::UnknownKey {
            name: raw_name.to_string(),
            key: unknown.clone(),
            expected: KNOWN_KEYS.join(", "),
        });
    }

    // `enabled` defaults to false: declaring an integration must never be
    // enough to turn it on.
    let enabled = match table.get("enabled") {
        None => false,
        Some(Value::Boolean(b)) => *b,
        Some(other) => {
            return Err(ParseError::WrongType {
                name: raw_name.to_string(),
                key: "enabled",
                expected: "a boolean",
                found: type_of(other),
            });
        }
    };

    let command = string_field(raw_name, table, "command")?;
    let root = string_field(raw_name, table, "root")?;

    let endpoint = match (command, root) {
        (Some(_), Some(_)) => {
            return Err(ParseError::AmbiguousEndpoint {
                name: raw_name.to_string(),
            });
        }
        (Some(binary), None) => IntegrationEndpoint::Command {
            binary: PathBuf::from(binary),
        },
        (None, Some(root)) => IntegrationEndpoint::Directory {
            root: PathBuf::from(root),
        },
        (None, None) => {
            return Err(ParseError::MissingEndpoint {
                name: raw_name.to_string(),
            });
        }
    };

    let capabilities = match table.get("capabilities") {
        None => Vec::new(),
        Some(Value::Array(items)) => {
            let mut caps = Vec::with_capacity(items.len());
            for item in items {
                let Value::String(s) = item else {
                    return Err(ParseError::WrongType {
                        name: raw_name.to_string(),
                        key: "capabilities",
                        expected: "an array of strings",
                        found: type_of(item),
                    });
                };
                // A blank label can never equal a real capability, so it would
                // gate nothing while looking configured.
                if s.trim().is_empty() {
                    return Err(ParseError::EmptyValue {
                        name: raw_name.to_string(),
                        key: "capabilities",
                    });
                }
                caps.push(s.trim().to_string());
            }
            caps
        }
        Some(other) => {
            return Err(ParseError::WrongType {
                name: raw_name.to_string(),
                key: "capabilities",
                expected: "an array of strings",
                found: type_of(other),
            });
        }
    };

    Ok(Integration {
        name,
        enabled,
        endpoint,
        capabilities,
    })
}

/// Read an optional string field, rejecting a present-but-empty value.
fn string_field(
    name: &str,
    table: &toml::Table,
    key: &'static str,
) -> Result<Option<String>, ParseError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Err(ParseError::EmptyValue {
            name: name.to_string(),
            key,
        }),
        // Deliberately NOT trimmed. A path is taken exactly as written, because
        // a filename may legitimately begin or end with a space, and silently
        // trimming would confine to — or execute — something other than what
        // the operator quoted. Only the all-blank case is refused, above.
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(ParseError::WrongType {
            name: name.to_string(),
            key,
            expected: "a string",
            found: type_of(other),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_without_integrations_yields_an_empty_set() {
        // The default posture. Note this document has *other* content, so the
        // test proves absence of `integrations` is handled, not absence of
        // configuration generally.
        let set = parse_integrations("provider = \"openrouter\"\n").expect("valid TOML parses");
        assert!(
            set.is_empty(),
            "no `integrations` table means none declared"
        );
    }

    #[test]
    fn an_empty_document_yields_an_empty_set() {
        assert!(parse_integrations("").expect("empty parses").is_empty());
    }

    #[test]
    fn a_declared_integration_defaults_to_disabled() {
        // The single most important default in this crate: writing the block
        // must not be the same act as switching it on.
        let set = parse_integrations("[integrations.beads]\ncommand = \"bd\"\n")
            .expect("a command endpoint is valid");

        let name = IntegrationName::new("beads").expect("legal name");
        let beads = set.get(&name).expect("declared");
        assert!(
            !beads.enabled,
            "an integration with no `enabled` key must default to OFF"
        );
        assert_eq!(set.active().count(), 0);
    }

    #[test]
    fn both_endpoint_shapes_parse() {
        let set = parse_integrations(
            "[integrations.beads]\ncommand = \"bd\"\n\
             [integrations.obsidian]\nroot = \"/vault\"\n",
        )
        .expect("both shapes are valid");

        assert_eq!(
            set.get(&IntegrationName::new("beads").unwrap())
                .unwrap()
                .endpoint,
            IntegrationEndpoint::Command {
                binary: PathBuf::from("bd")
            }
        );
        assert_eq!(
            set.get(&IntegrationName::new("obsidian").unwrap())
                .unwrap()
                .endpoint,
            IntegrationEndpoint::Directory {
                root: PathBuf::from("/vault")
            }
        );
    }

    #[test]
    fn an_unknown_key_fails_the_load_rather_than_being_ignored() {
        // `enable` instead of `enabled` is the exact mistake this guards: an
        // ignored key would leave the operator believing it took effect.
        let err = parse_integrations("[integrations.beads]\ncommand = \"bd\"\nenable = true\n")
            .expect_err("an unknown key must fail the load");

        match err {
            ParseError::UnknownKey { name, key, .. } => {
                assert_eq!(name, "beads");
                assert_eq!(key, "enable");
            }
            other => panic!("expected UnknownKey, got {other:?}"),
        }
    }

    #[test]
    fn an_integration_must_declare_exactly_one_endpoint() {
        let missing = parse_integrations("[integrations.beads]\nenabled = true\n")
            .expect_err("no endpoint must fail");
        assert!(matches!(missing, ParseError::MissingEndpoint { .. }));

        // Both is refused rather than picking one, because the confinement
        // rules differ and silently preferring one would confine differently
        // than the operator's other line implies.
        let both =
            parse_integrations("[integrations.beads]\ncommand = \"bd\"\nroot = \"/vault\"\n")
                .expect_err("two endpoints must fail");
        assert!(matches!(both, ParseError::AmbiguousEndpoint { .. }));
    }

    #[test]
    fn wrong_types_are_refused_with_the_offending_key_named() {
        let err = parse_integrations("[integrations.beads]\ncommand = \"bd\"\nenabled = \"yes\"\n")
            .expect_err("a string is not a boolean");

        match err {
            ParseError::WrongType { key, found, .. } => {
                assert_eq!(key, "enabled");
                assert_eq!(found, "a string");
            }
            other => panic!("expected WrongType, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_endpoint_or_capability_is_refused() {
        // A blank command would resolve to nothing; a blank capability label
        // can never equal a real one, so it gates nothing while looking set.
        assert!(matches!(
            parse_integrations("[integrations.beads]\ncommand = \"   \"\n")
                .expect_err("blank command must fail"),
            ParseError::EmptyValue { key: "command", .. }
        ));
        assert!(matches!(
            parse_integrations("[integrations.beads]\ncommand = \"bd\"\ncapabilities = [\"\"]\n")
                .expect_err("blank capability must fail"),
            ParseError::EmptyValue {
                key: "capabilities",
                ..
            }
        ));
    }

    #[test]
    fn an_illegal_name_is_refused_at_the_block_level() {
        let err = parse_integrations("[integrations.Beads]\ncommand = \"bd\"\n")
            .expect_err("uppercase names are refused");
        assert!(matches!(err, ParseError::Name { .. }));
    }

    #[test]
    fn invalid_toml_reports_a_toml_error_rather_than_panicking() {
        let err = parse_integrations("[integrations.beads\ncommand =").expect_err("malformed");
        assert!(matches!(err, ParseError::Toml { .. }));
    }
}

#[cfg(test)]
mod path_fidelity_tests {
    use super::*;

    /// A path is taken exactly as written.
    ///
    /// A filename may legitimately begin or end with a space. Trimming would
    /// confine to, or execute, something other than what the operator quoted —
    /// in the directory case potentially a sibling tree.
    #[test]
    fn endpoint_paths_keep_their_leading_and_trailing_spaces() {
        let set = parse_integrations(
            "[integrations.obsidian]\nroot = \" /vault \"\n\
             [integrations.beads]\ncommand = \" bd \"\n",
        )
        .expect("padded paths are valid");

        assert_eq!(
            set.get(&IntegrationName::new("obsidian").unwrap())
                .unwrap()
                .endpoint,
            IntegrationEndpoint::Directory {
                root: PathBuf::from(" /vault ")
            },
            "a root must be confined exactly as written, not silently retargeted"
        );
        assert_eq!(
            set.get(&IntegrationName::new("beads").unwrap())
                .unwrap()
                .endpoint,
            IntegrationEndpoint::Command {
                binary: PathBuf::from(" bd ")
            }
        );
    }
}
