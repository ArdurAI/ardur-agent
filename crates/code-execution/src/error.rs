//! The crate's typed-error surface.

/// Every way a [`crate::CodeExecutionTool`] dispatch can fail before it
/// reaches [`ardur_tool_registry::ToolError`].
#[derive(Debug, thiserror::Error)]
pub enum CodeExecutionError {
    /// The requested `language` is not one of the closed [`LanguageAdapter`](crate::LanguageAdapter)
    /// impls this crate ships.
    #[error("unsupported language: {0}")]
    UnsupportedLanguage(String),

    /// The requested `language` is not in the cap-token caveat's
    /// `permitted_languages` set.
    #[error("language `{0}` is not permitted by the caller's cap-token caveat")]
    LanguageNotPermitted(String),

    /// The requested `tool_allowlist` is not a subset of the cap-token
    /// caveat's permitted tools.
    #[error("tool `{0}` is not in the caller's permitted tool set")]
    ToolNotPermitted(String),

    /// The requested `timeout_secs` exceeds the cap-token caveat's ceiling.
    #[error("requested timeout {requested}s exceeds the caveat ceiling of {ceiling}s")]
    TimeoutCeilingExceeded {
        /// What the caller asked for.
        requested: u64,
        /// The caveat's maximum.
        ceiling: u64,
    },

    /// The child process could not be spawned.
    #[error("failed to spawn `{language}` adapter: {source}")]
    Spawn {
        /// The language adapter that failed to spawn.
        language: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The child process did not exit within its wall-clock ceiling.
    #[error("execution timed out after {0}s")]
    Timeout(u64),

    /// The prompt-injection filter blocked the captured output before it
    /// could be returned to the caller.
    #[error("captured output blocked by injection filter: {0}")]
    InjectionBlocked(String),
}
