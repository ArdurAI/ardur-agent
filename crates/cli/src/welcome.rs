//! §2.X first-launch welcome splash (design §B.1, §H).
//!
//! The brand appears at exactly one moment in the session: the first time `ardur`
//! is run, gated on a small state file. The logo (the "bar mark", design Variant
//! 2) prints in the primary colour, taglines in dim, then the bit flips so it
//! never shows again. `/about` and `--version` reprint it deliberately;
//! mid-session it is never reprinted (restraint).

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::theme::{Role, Theme};

/// The "bar mark" logo (design §B.1, Variant 2 — selected).
const LOGO: &str = r"        _
   __ _ _ __ __| |_   _ _ __
  / _` | '__/ _` | | | | '__|
 | (_| | | | (_| | |_| | |
  \__,_|_|  \__,_|\__,_|_|";

/// The tagline under the logo.
const TAGLINE: &str = "the agent that keeps the receipts";

/// The provenance sub-tagline.
const SUBTAGLINE: &str = "every action signed · every memory provable";

/// The conventional state-file location: `~/.config/ardur/state.toml`. `None` if
/// no home directory resolves.
#[must_use]
pub fn default_state_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .map(|base| base.join("ardur").join("state.toml"))
}

/// Render the splash (logo + taglines + start hint) through `theme`.
#[must_use]
pub fn splash(theme: &Theme) -> String {
    let logo = theme.paint(Role::Primary, LOGO);
    let tagline = theme.paint(Role::Dim, TAGLINE);
    let sub = theme.paint(Role::Dim, SUBTAGLINE);
    let hint = theme.paint(Role::Dim, "press Enter to begin");
    format!("\n{logo}\n\n        {tagline}\n\n        {sub}\n\n        {hint}\n")
}

/// Show the splash to `out` iff this is the first launch (per `state_path`), then
/// persist the flag so it never shows again. Returns `true` when the splash was
/// shown.
///
/// A missing or unreadable state file is treated as a first launch; after showing,
/// the directory is created and `first_launch = false` is written. A write failure
/// is surfaced (so a read-only home doesn't silently re-show every run).
pub fn show_welcome_if_first<W: Write>(
    state_path: &Path,
    theme: &Theme,
    out: &mut W,
) -> std::io::Result<bool> {
    if !is_first_launch(state_path) {
        return Ok(false);
    }
    write!(out, "{}", splash(theme))?;
    out.flush()?;
    persist_welcomed(state_path)?;
    Ok(true)
}

/// Whether `state_path` indicates a first launch (absent, unreadable, or holding
/// `first_launch = true`).
#[must_use]
pub fn is_first_launch(state_path: &Path) -> bool {
    match std::fs::read_to_string(state_path) {
        Ok(contents) => !contents.lines().any(|line| {
            let line = line.split('#').next().unwrap_or("").trim();
            matches!(
                line.split_once('=').map(|(k, v)| (k.trim(), v.trim())),
                Some(("first_launch", "false"))
            )
        }),
        Err(_) => true,
    }
}

/// Write the state file marking the welcome as shown.
fn persist_welcomed(state_path: &Path) -> std::io::Result<()> {
    if let Some(parent) = state_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(state_path, "# ardur CLI state\nfirst_launch = false\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeName;

    #[test]
    fn splash_contains_brand() {
        let s = splash(&Theme::named(ThemeName::Night).plain());
        assert!(s.contains("the agent that keeps the receipts"));
        assert!(s.contains(r"\__,_|")); // a logo row
    }

    #[test]
    fn shows_once_then_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let theme = Theme::named(ThemeName::Night).plain();

        let mut first = Vec::new();
        assert!(show_welcome_if_first(&path, &theme, &mut first).unwrap());
        assert!(!first.is_empty());
        assert!(path.exists());

        let mut second = Vec::new();
        assert!(!show_welcome_if_first(&path, &theme, &mut second).unwrap());
        assert!(second.is_empty());
    }
}
