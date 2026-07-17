//! §2.X Phase-1 slash-command surface (design §F, Phase-1 subset).
//!
//! Phase 1 ships `/help`, `/clear`, `/exit` (alias `/quit`), `/theme <name>`,
//! `/cost`, and the pre-existing `/budget`. The richer set (`/sessions`,
//! `/memory`, `/receipts`, `/skill`, `/copy`, `/save`, …) is deferred to Phases
//! 2–3. This module owns the single canonical help text (so the REPL and the
//! `/help` bus command never drift) and the live `/theme` switch logic.

use crate::theme::{Theme, ThemeName};

/// The canonical Phase-1 command reference, rendered by `/help` in the REPL and
/// the bus.
#[must_use]
pub fn phase1_help() -> String {
    [
        "Commands:",
        "  /help            show this help",
        "  /clear           clear the screen",
        "  /theme <name>    switch theme live (dawn · night · terminal)",
        "  /cost            show this session's running cost",
        "  /budget          show the remaining session budget",
        "  /memory list [--json]  list scoped memory cards",
        "  /memory show <id>      show memory provenance and payload",
        "  /memory forget <id>    append a receipt-linked tombstone",
        "  /quit, /exit     leave the chat",
        "Type anything else to send it as a chat message.",
    ]
    .join("\n")
}

/// Apply a `/theme` command, switching `current` in place. An empty argument
/// lists the choices; a known name switches (preserving the styled/`NO_COLOR`
/// state); an unknown name is an error.
///
/// Returns the line to print on success, or an error line for an unknown theme.
pub fn apply_theme_command(arg: &str, current: &mut Theme) -> Result<String, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Ok(format!(
            "theme: {} · choose: dawn · night · terminal",
            current.name().as_str()
        ));
    }
    let name = ThemeName::parse(arg)
        .ok_or_else(|| format!("unknown theme '{arg}' — try: dawn, night, terminal"))?;
    let styled = current.is_styled();
    *current = if styled {
        Theme::named(name)
    } else {
        Theme::named(name).plain()
    };
    Ok(format!("theme → {}", name.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_lists_phase1_commands() {
        let help = phase1_help();
        for cmd in ["/help", "/clear", "/theme", "/cost", "/quit"] {
            assert!(help.contains(cmd), "help missing {cmd}");
        }
    }

    #[test]
    fn theme_switch_and_unknown() {
        let mut theme = Theme::named(ThemeName::Dawn);
        let msg = apply_theme_command("night", &mut theme).unwrap();
        assert_eq!(theme.name(), ThemeName::Night);
        assert!(msg.contains("night"));

        assert!(apply_theme_command("bogus", &mut theme).is_err());
        // unchanged after a failed switch
        assert_eq!(theme.name(), ThemeName::Night);
    }

    #[test]
    fn theme_switch_preserves_unstyled() {
        let mut theme = Theme::named(ThemeName::Night).plain();
        apply_theme_command("dawn", &mut theme).unwrap();
        assert_eq!(theme.name(), ThemeName::Dawn);
        assert!(!theme.is_styled());
    }
}
