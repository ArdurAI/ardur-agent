//! [`CodeExecutionCaveat`] — the operator/cap-token-declared ceiling every
//! `code.exec` request is attenuated against before an adapter runs.
//!
//! Per the §6.7 blueprint's Differentiation Note 2, Hermes's script can call
//! any tool registered in its parent process. Ardur narrows this: the
//! caller's per-call `tool_allowlist` is a *stated intent*, the caveat's
//! `permitted_tools` is the *operator ceiling*, and only the intersection is
//! ever honoured.
//!
//! Phase 2 mints this caveat from a verified cap-token's Biscuit block (see
//! `ardur-cap-token`); Phase 1 constructs it directly (e.g. from `ToolContext`
//! or a caller-supplied default) since cap-token-to-caveat projection is not
//! wired into this crate yet.

use serde::{Deserialize, Serialize};

use crate::error::CodeExecutionError;
use crate::tool::CodeExecutionRequest;

/// The ceilings a `code.exec` dispatch is attenuated against.
///
/// Every field narrows (never widens) what a bare [`CodeExecutionRequest`]
/// asks for — `attenuate` never grants more than the request declared.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CodeExecutionCaveat {
    /// The maximum wall-clock ceiling any single dispatch may run for,
    /// regardless of what the request's `timeout_secs` asks for.
    pub max_timeout_secs: u64,
    /// The languages this caveat permits. A request naming a language
    /// outside this set is rejected before any adapter spawns.
    pub permitted_languages: Vec<String>,
    /// The tools this caveat permits the script to declare an intent to call
    /// back into. The request's `tool_allowlist` is intersected against this
    /// set; anything outside it is silently dropped and receipted as denied.
    pub permitted_tools: Vec<String>,
    /// When `true`, forces `expose_stderr = false` on every dispatch this
    /// caveat governs, regardless of what the request asks for — the
    /// operator-enforced override described in Differentiation Note 6.
    pub force_stderr_hidden: bool,
    /// The maximum captured-output size, in bytes, before the tool truncates
    /// stdout/stderr.
    pub max_output_bytes: usize,
}

impl CodeExecutionCaveat {
    /// A permissive Phase-1 default: five-minute ceiling, both shipped
    /// adapters permitted, no tool callbacks, stderr exposure left to the
    /// caller, 256 KiB output ceiling.
    ///
    /// Intended for local development and tests only — production callers
    /// should construct a caveat from the operator's actual cap-token grant
    /// once §11.0 cap-token-to-caveat projection lands.
    #[must_use]
    pub fn permissive_default() -> Self {
        Self {
            max_timeout_secs: 300,
            permitted_languages: vec!["bash".to_string(), "python".to_string()],
            permitted_tools: Vec::new(),
            force_stderr_hidden: false,
            max_output_bytes: 256 * 1024,
        }
    }

    /// Attenuate `request` against this caveat, returning the narrowed
    /// request the adapter actually runs, or the first violated ceiling.
    pub fn attenuate(
        &self,
        request: &CodeExecutionRequest,
    ) -> Result<CodeExecutionRequest, CodeExecutionError> {
        if !self
            .permitted_languages
            .iter()
            .any(|lang| lang == &request.language)
        {
            return Err(CodeExecutionError::LanguageNotPermitted(
                request.language.clone(),
            ));
        }

        if request.timeout_secs > self.max_timeout_secs {
            return Err(CodeExecutionError::TimeoutCeilingExceeded {
                requested: request.timeout_secs,
                ceiling: self.max_timeout_secs,
            });
        }

        let (allowed_tools, denied_tools): (Vec<String>, Vec<String>) = request
            .tool_allowlist
            .iter()
            .cloned()
            .partition(|tool| self.permitted_tools.iter().any(|t| t == tool));

        let mut narrowed = request.clone();
        narrowed.tool_allowlist = allowed_tools;
        narrowed.denied_tools = denied_tools;
        if self.force_stderr_hidden {
            narrowed.expose_stderr = false;
        }
        Ok(narrowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::CodeExecutionRequest;

    fn request(language: &str) -> CodeExecutionRequest {
        CodeExecutionRequest {
            language: language.to_string(),
            code: "echo hi".to_string(),
            stdin: None,
            timeout_secs: 10,
            tool_allowlist: vec!["fs.read".to_string(), "shell.run".to_string()],
            denied_tools: Vec::new(),
            expose_stdout: true,
            expose_stderr: true,
        }
    }

    #[test]
    fn rejects_unpermitted_language() {
        let caveat = CodeExecutionCaveat {
            permitted_languages: vec!["bash".to_string()],
            ..CodeExecutionCaveat::permissive_default()
        };
        let err = caveat.attenuate(&request("python")).unwrap_err();
        assert!(matches!(err, CodeExecutionError::LanguageNotPermitted(_)));
    }

    #[test]
    fn rejects_timeout_above_ceiling() {
        let caveat = CodeExecutionCaveat {
            max_timeout_secs: 5,
            ..CodeExecutionCaveat::permissive_default()
        };
        let err = caveat.attenuate(&request("bash")).unwrap_err();
        assert!(matches!(
            err,
            CodeExecutionError::TimeoutCeilingExceeded { .. }
        ));
    }

    #[test]
    fn intersects_tool_allowlist_and_records_denials() {
        let caveat = CodeExecutionCaveat {
            permitted_tools: vec!["fs.read".to_string()],
            ..CodeExecutionCaveat::permissive_default()
        };
        let narrowed = caveat.attenuate(&request("bash")).expect("attenuates");
        assert_eq!(narrowed.tool_allowlist, vec!["fs.read".to_string()]);
        assert_eq!(narrowed.denied_tools, vec!["shell.run".to_string()]);
    }

    #[test]
    fn forces_stderr_hidden_when_caveat_demands_it() {
        let caveat = CodeExecutionCaveat {
            force_stderr_hidden: true,
            ..CodeExecutionCaveat::permissive_default()
        };
        let narrowed = caveat.attenuate(&request("bash")).expect("attenuates");
        assert!(!narrowed.expose_stderr);
    }

    #[test]
    fn never_widens_tool_allowlist_beyond_the_request() {
        let mut caveat = CodeExecutionCaveat::permissive_default();
        caveat.permitted_tools = vec![
            "fs.read".to_string(),
            "shell.run".to_string(),
            "http.fetch".to_string(),
        ];
        let narrowed = caveat.attenuate(&request("bash")).expect("attenuates");
        // The request only declared fs.read + shell.run — the caveat
        // permitting http.fetch too must not inject it into the result.
        assert_eq!(narrowed.tool_allowlist.len(), 2);
    }
}
