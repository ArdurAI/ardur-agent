//! OSC-8 hyperlink support detection (design §C, §I).
//!
//! Ardur emits OSC-8 hyperlinks *only* where the terminal advertises support;
//! everywhere else links render as `text (url)` so the destination is never
//! hidden. Detection is a conservative allowlist over the terminal-identifying
//! environment, read through an injected lookup so it stays hermetic (the crate
//! forbids the edition-2024 `unsafe` `set_var`).

/// The terminal-program identifiers known to render OSC-8 hyperlinks.
const OSC8_TERM_PROGRAMS: &[&str] = &["iTerm.app", "WezTerm", "vscode", "ghostty", "rio"];

/// Whether the terminal described by `lookup` supports OSC-8 hyperlinks.
///
/// Conservative: a known `TERM_PROGRAM`, a kitty/`xterm`-with-`VTE` terminal, or
/// an explicit `VTE_VERSION ≥ 0.50.0`. Unknown terminals are treated as
/// unsupported (the `text (url)` fallback is always safe).
pub fn terminal_supports_osc8<F>(lookup: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    // An explicit force flag wins (operators/testing).
    if let Some(v) = lookup("ARDUR_FORCE_OSC8") {
        return matches!(v.trim(), "1" | "true" | "yes");
    }
    if let Some(tp) = lookup("TERM_PROGRAM") {
        if OSC8_TERM_PROGRAMS
            .iter()
            .any(|k| k.eq_ignore_ascii_case(&tp))
        {
            return true;
        }
    }
    if let Some(term) = lookup("TERM") {
        if term.contains("kitty") {
            return true;
        }
    }
    // GNOME-VTE terminals (gnome-terminal, tilix, …) gained OSC-8 in VTE 0.50.
    if let Some(vte) = lookup("VTE_VERSION") {
        if let Ok(v) = vte.trim().parse::<u32>() {
            return v >= 5000; // VTE encodes 0.50.0 as 5000
        }
    }
    false
}

/// Detect OSC-8 support from the process environment.
#[must_use]
pub fn osc8_from_env() -> bool {
    terminal_supports_osc8(|key| std::env::var(key).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_programs_supported() {
        assert!(terminal_supports_osc8(
            |k| (k == "TERM_PROGRAM").then(|| "iTerm.app".to_string())
        ));
        assert!(terminal_supports_osc8(
            |k| (k == "TERM").then(|| "xterm-kitty".to_string())
        ));
        assert!(terminal_supports_osc8(
            |k| (k == "VTE_VERSION").then(|| "6003".to_string())
        ));
    }

    #[test]
    fn unknown_unsupported_and_force_flag() {
        assert!(!terminal_supports_osc8(|_| None));
        assert!(!terminal_supports_osc8(
            |k| (k == "TERM").then(|| "dumb".to_string())
        ));
        assert!(terminal_supports_osc8(
            |k| (k == "ARDUR_FORCE_OSC8").then(|| "1".to_string())
        ));
    }
}
