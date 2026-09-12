//! `ARDUR_INTEGRATIONS_*` environment overrides.
//!
//! The file declares integrations; the environment adjusts them. That split is
//! deliberate: a container image can ship a config file describing what an
//! integration *is*, and a deployment can decide whether it is *on* without
//! rewriting the file.
//!
//! # What the environment may and may not do
//!
//! An override can toggle `enabled` and retarget an endpoint. It cannot
//! **introduce** an integration that the file never declared.
//!
//! That restriction is the point of the module. If `ARDUR_INTEGRATIONS_FOO_ROOT`
//! could conjure an integration, then an environment variable — the part of a
//! deployment most likely to be inherited, templated, or set by a parent
//! process — would be enough to grant the runtime a new confined directory that
//! no reviewed configuration file ever mentioned. Requiring a declaration keeps
//! the set of *possible* integrations in a file someone reviews, and leaves the
//! environment in charge only of the ones already there.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::{IntegrationEndpoint, IntegrationName, IntegrationSet, NameError};

/// The prefix every override shares.
const PREFIX: &str = "ARDUR_INTEGRATIONS_";

/// Why an environment override was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvOverrideError {
    /// The variable did not end in a recognised suffix.
    #[error(
        "`{var}` is not a recognised integration override; expected \
         `ARDUR_INTEGRATIONS_<NAME>_ENABLED`, `_COMMAND`, or `_ROOT`"
    )]
    UnknownSuffix {
        /// The offending variable name.
        var: String,
    },
    /// The `<NAME>` portion was not a usable integration name.
    #[error("`{var}` names integration `{name}`, which is not usable: {source}")]
    Name {
        /// The offending variable name.
        var: String,
        /// The derived integration name.
        name: String,
        /// Why it was refused.
        #[source]
        source: NameError,
    },
    /// The variable referred to an integration that no file declared.
    #[error(
        "`{var}` refers to integration `{name}`, which is not declared in the \
         configuration file; the environment may enable or retarget a declared \
         integration but cannot introduce one, so that the set of possible \
         integrations stays in reviewed configuration"
    )]
    Undeclared {
        /// The offending variable name.
        var: String,
        /// The integration it referred to.
        name: String,
    },
    /// Two declared names collide in the environment-override namespace.
    #[error(
        "`{var}` is ambiguous: integrations `{first}` and `{second}` both map to \
         `{PREFIX}{infix}_*`, so this variable cannot say which one it means; \
         rename one of them"
    )]
    AmbiguousName {
        /// The offending variable name.
        var: String,
        /// The env infix both names share.
        infix: String,
        /// One colliding declaration.
        first: String,
        /// The other colliding declaration.
        second: String,
    },
    /// `_ENABLED` held something other than a boolean.
    #[error("`{var}` must be `true` or `false` (case-insensitive), found `{value}`")]
    NotABoolean {
        /// The offending variable name.
        var: String,
        /// The value found. Safe to echo: this variable is a boolean flag, not
        /// a credential.
        value: String,
    },
    /// A value was empty or blank.
    #[error("`{var}` cannot be empty")]
    Empty {
        /// The offending variable name.
        var: String,
    },
    /// The override's shape disagreed with the declared endpoint.
    #[error(
        "`{var}` sets {attempted} for integration `{name}`, but it is declared \
         as {declared}; an override may retarget an endpoint, not change its kind"
    )]
    EndpointKindMismatch {
        /// The offending variable name.
        var: String,
        /// The integration name.
        name: String,
        /// What the override tried to set.
        attempted: &'static str,
        /// What the file declared.
        declared: &'static str,
    },
}

/// Apply every `ARDUR_INTEGRATIONS_*` variable in `vars` to `set`.
///
/// `vars` is passed in rather than read from the process environment so the
/// behaviour is testable without mutating global state — `std::env::set_var` is
/// process-wide and racy under a parallel test runner.
///
/// Variables outside the `ARDUR_INTEGRATIONS_` prefix are ignored, so the whole
/// environment can be handed to this function.
///
/// # Errors
///
/// Returns [`EnvOverrideError`] if a variable has an unrecognised suffix, names
/// an undeclared or unusable integration, carries a malformed value, or tries
/// to change an endpoint's kind.
pub fn apply_env_overrides(
    set: &mut IntegrationSet,
    vars: &BTreeMap<String, String>,
) -> Result<(), EnvOverrideError> {
    for (var, value) in vars {
        let Some(rest) = var.strip_prefix(PREFIX) else {
            continue;
        };

        // Split on the LAST underscore: the name may itself contain one
        // (`my_tool`), but the suffix never does.
        let Some((raw_name, suffix)) = rest.rsplit_once('_') else {
            return Err(EnvOverrideError::UnknownSuffix { var: var.clone() });
        };

        // The env form is upper-cased with `-` mapped to `_`; the declared form
        // is lower-case. Lower-casing recovers `my_tool` but cannot distinguish
        // it from `my-tool`, so try the literal reading first and fall back to
        // the hyphenated one.
        let lowered = raw_name.to_ascii_lowercase();
        let name = match resolve_declared_name(set, &lowered, var)? {
            Some(name) => name,
            None => {
                return Err(match IntegrationName::new(lowered.clone()) {
                    Ok(_) => EnvOverrideError::Undeclared {
                        var: var.clone(),
                        name: lowered,
                    },
                    Err(source) => EnvOverrideError::Name {
                        var: var.clone(),
                        name: lowered,
                        source,
                    },
                });
            }
        };

        let trimmed = value.trim();

        match suffix {
            "ENABLED" => {
                let enabled = match trimmed.to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" | "on" => true,
                    "false" | "0" | "no" | "off" => false,
                    _ => {
                        return Err(EnvOverrideError::NotABoolean {
                            var: var.clone(),
                            value: trimmed.to_string(),
                        });
                    }
                };
                let entry = set.get_mut(&name).expect("resolved above");
                entry.enabled = enabled;
            }
            "COMMAND" | "ROOT" => {
                if trimmed.is_empty() {
                    return Err(EnvOverrideError::Empty { var: var.clone() });
                }
                let entry = set.get_mut(&name).expect("resolved above");
                match (&entry.endpoint, suffix) {
                    (IntegrationEndpoint::Command { .. }, "COMMAND") => {
                        entry.endpoint = IntegrationEndpoint::Command {
                            binary: PathBuf::from(trimmed),
                        };
                    }
                    (IntegrationEndpoint::Directory { .. }, "ROOT") => {
                        entry.endpoint = IntegrationEndpoint::Directory {
                            root: PathBuf::from(trimmed),
                        };
                    }
                    // Changing kind would change which confinement rules apply,
                    // which is a different integration wearing the same name.
                    (declared, attempted) => {
                        return Err(EnvOverrideError::EndpointKindMismatch {
                            var: var.clone(),
                            name: name.to_string(),
                            attempted: if attempted == "COMMAND" {
                                "a command"
                            } else {
                                "a root directory"
                            },
                            declared: match declared {
                                IntegrationEndpoint::Command { .. } => "a command",
                                IntegrationEndpoint::Directory { .. } => "a root directory",
                            },
                        });
                    }
                }
            }
            _ => {
                return Err(EnvOverrideError::UnknownSuffix { var: var.clone() });
            }
        }
    }
    Ok(())
}

/// Find the declared name matching a lower-cased env infix.
///
/// `MY_TOOL` could mean `my_tool` or `my-tool`; only the declaration says
/// which, so both readings are checked against what was actually declared.
///
/// If *both* are declared the variable is genuinely ambiguous. Silently
/// preferring the underscore reading would make the hyphenated integration
/// impossible to override while looking as though the override applied, so the
/// collision is reported instead.
fn resolve_declared_name(
    set: &IntegrationSet,
    lowered: &str,
    var: &str,
) -> Result<Option<IntegrationName>, EnvOverrideError> {
    let direct = IntegrationName::new(lowered)
        .ok()
        .filter(|n| set.get(n).is_some());
    let hyphenated = IntegrationName::new(lowered.replace('_', "-"))
        .ok()
        .filter(|n| set.get(n).is_some());

    match (direct, hyphenated) {
        (Some(a), Some(b)) if a != b => Err(EnvOverrideError::AmbiguousName {
            var: var.to_string(),
            infix: lowered.to_ascii_uppercase(),
            first: a.to_string(),
            second: b.to_string(),
        }),
        (Some(a), _) => Ok(Some(a)),
        (None, Some(b)) => Ok(Some(b)),
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Integration, parse_integrations};

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn declared() -> IntegrationSet {
        parse_integrations(
            "[integrations.beads]\ncommand = \"bd\"\n\
             [integrations.obsidian]\nroot = \"/vault\"\n",
        )
        .expect("fixture parses")
    }

    #[test]
    fn the_environment_can_enable_a_declared_integration() {
        let mut set = declared();
        assert_eq!(set.active().count(), 0, "both start disabled");

        apply_env_overrides(
            &mut set,
            &vars(&[("ARDUR_INTEGRATIONS_BEADS_ENABLED", "true")]),
        )
        .expect("enabling a declared integration is allowed");

        assert_eq!(set.active().count(), 1);
        assert!(
            set.get(&IntegrationName::new("beads").unwrap())
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn the_environment_cannot_introduce_an_undeclared_integration() {
        // The security property of this module. An inherited or templated
        // variable must not be able to grant the runtime a confined directory
        // that no reviewed file mentions.
        let mut set = declared();
        let err = apply_env_overrides(
            &mut set,
            &vars(&[("ARDUR_INTEGRATIONS_SECRETS_ROOT", "/etc")]),
        )
        .expect_err("an undeclared integration must be refused");

        match err {
            EnvOverrideError::Undeclared { name, .. } => assert_eq!(name, "secrets"),
            other => panic!("expected Undeclared, got {other:?}"),
        }
        assert_eq!(set.len(), 2, "nothing may be added to the set");
    }

    #[test]
    fn an_override_may_retarget_an_endpoint_but_not_change_its_kind() {
        let mut set = declared();

        apply_env_overrides(
            &mut set,
            &vars(&[("ARDUR_INTEGRATIONS_BEADS_COMMAND", "/opt/homebrew/bin/bd")]),
        )
        .expect("retargeting a command is allowed");
        assert_eq!(
            set.get(&IntegrationName::new("beads").unwrap())
                .unwrap()
                .endpoint,
            IntegrationEndpoint::Command {
                binary: PathBuf::from("/opt/homebrew/bin/bd")
            }
        );

        // `beads` is a command; setting a ROOT on it would silently switch
        // which confinement rules apply.
        let err = apply_env_overrides(
            &mut set,
            &vars(&[("ARDUR_INTEGRATIONS_BEADS_ROOT", "/vault")]),
        )
        .expect_err("changing kind must be refused");
        assert!(matches!(err, EnvOverrideError::EndpointKindMismatch { .. }));
    }

    #[test]
    fn a_name_containing_an_underscore_resolves_to_its_declaration() {
        // `MY_TOOL` is ambiguous between `my_tool` and `my-tool`; only the
        // declaration disambiguates. Here the hyphenated form is declared.
        let mut set = parse_integrations("[integrations.my-tool]\ncommand = \"mt\"\n")
            .expect("fixture parses");

        apply_env_overrides(
            &mut set,
            &vars(&[("ARDUR_INTEGRATIONS_MY_TOOL_ENABLED", "true")]),
        )
        .expect("the hyphenated declaration must be found");

        assert!(
            set.get(&IntegrationName::new("my-tool").unwrap())
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn a_malformed_boolean_is_refused_rather_than_treated_as_false() {
        // Treating `ture` as false would silently leave an integration off that
        // the operator believes they enabled.
        let mut set = declared();
        let err = apply_env_overrides(
            &mut set,
            &vars(&[("ARDUR_INTEGRATIONS_BEADS_ENABLED", "ture")]),
        )
        .expect_err("a typo'd boolean must fail");
        assert!(matches!(err, EnvOverrideError::NotABoolean { .. }));
    }

    #[test]
    fn an_unknown_suffix_is_refused() {
        let mut set = declared();
        let err = apply_env_overrides(
            &mut set,
            &vars(&[("ARDUR_INTEGRATIONS_BEADS_TIMEOUT", "30")]),
        )
        .expect_err("an unrecognised suffix must fail");
        assert!(matches!(err, EnvOverrideError::UnknownSuffix { .. }));
    }

    #[test]
    fn unrelated_variables_are_ignored() {
        // The whole process environment may be handed to this function.
        let mut set = declared();
        apply_env_overrides(
            &mut set,
            &vars(&[("PATH", "/usr/bin"), ("ARDUR_COST_BUDGET_CENTS", "500")]),
        )
        .expect("unrelated variables must be ignored");
        assert_eq!(set.active().count(), 0);
    }

    #[test]
    fn overrides_apply_in_a_deterministic_order() {
        // A BTreeMap iterates in sorted order, so two overrides of the same
        // field cannot produce a host-dependent result.
        let mut set = IntegrationSet::new();
        set.insert(Integration {
            name: IntegrationName::new("beads").unwrap(),
            enabled: false,
            endpoint: IntegrationEndpoint::Command {
                binary: PathBuf::from("bd"),
            },
            capabilities: vec![],
        });

        apply_env_overrides(
            &mut set,
            &vars(&[
                ("ARDUR_INTEGRATIONS_BEADS_COMMAND", "/a/bd"),
                ("ARDUR_INTEGRATIONS_BEADS_ENABLED", "1"),
            ]),
        )
        .expect("both apply");

        let beads = set.get(&IntegrationName::new("beads").unwrap()).unwrap();
        assert!(beads.enabled);
        assert_eq!(
            beads.endpoint,
            IntegrationEndpoint::Command {
                binary: PathBuf::from("/a/bd")
            }
        );
    }
}

#[cfg(test)]
mod collision_tests {
    use super::*;
    use crate::parse_integrations;
    use std::collections::BTreeMap;

    /// Two declarations that share an env namespace make the variable
    /// ambiguous. Silently preferring one would make the other impossible to
    /// override while looking as though the override had applied.
    #[test]
    fn colliding_declarations_make_an_override_ambiguous_rather_than_silent() {
        let mut set = parse_integrations(
            "[integrations.my_tool]\ncommand = \"a\"\n\
             [integrations.my-tool]\ncommand = \"b\"\n",
        )
        .expect("both names are individually legal");

        let vars: BTreeMap<String, String> = [(
            "ARDUR_INTEGRATIONS_MY_TOOL_ENABLED".to_string(),
            "true".to_string(),
        )]
        .into_iter()
        .collect();

        let err = match apply_env_overrides(&mut set, &vars) {
            Err(e) => e,
            Ok(()) => panic!("a colliding override must be refused, not silently applied"),
        };

        match err {
            EnvOverrideError::AmbiguousName { first, second, .. } => {
                let mut names = [first, second];
                names.sort();
                assert_eq!(names, ["my-tool".to_string(), "my_tool".to_string()]);
            }
            other => panic!("expected AmbiguousName, got {other:?}"),
        }

        // Neither was touched: refusing must not half-apply.
        assert_eq!(set.active().count(), 0);
    }
}
