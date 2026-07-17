//! §2.X colour themes for the stunning-CLI rendering core (ADR-Phase2-021).
//!
//! A [`Theme`] is a named [`Palette`] plus a `styled` flag. Every renderer in the
//! Phase-1 core ([`markdown`](crate::markdown), [`toolbox`](crate::toolbox),
//! [`welcome`](crate::welcome)) paints through a `Theme` so colour, restraint, and
//! the `NO_COLOR` degradation are decided in exactly one place.
//!
//! Three themes ship (design §B.2 / §E):
//!
//! - [`dawn`](ThemeName::Dawn) — light, warm (ochre/teal on cream).
//! - [`night`](ThemeName::Night) — dark, cool (amber/sky on near-black). **Default**
//!   — the design doc's autodetect lands here for the dark terminals most users run
//!   (the §2.X roadmap pins `night` as the Phase-1 default).
//! - [`terminal`](ThemeName::Terminal) — zero override; roles map onto the
//!   terminal's own ANSI-16 palette so Ardur blends into a curated colour scheme.
//!
//! `dawn`/`night` emit ANSI-256 SGR codes (the doc's specified middle tier, legible
//! on the overwhelming majority of terminals); `terminal` emits ANSI-16 named
//! codes. When styling is off (`NO_COLOR`, a non-tty, or `--plain`) every paint is
//! the identity — the glyph/word pairing each renderer keeps (palette-law #1)
//! carries the meaning without colour.
//!
//! Selection (Phase 1): the `ARDUR_THEME` environment variable, then the default.
//! Live `/theme <name>` switching and `NO_COLOR` are read through
//! [`Theme::from_lookup`] so callers (and tests) inject the environment explicitly
//! — edition-2024 makes process-wide `set_var` `unsafe`, which this crate forbids.

/// The environment variable selecting the active theme (`dawn`|`night`|`terminal`).
pub const THEME_ENV: &str = "ARDUR_THEME";

/// The conventional variable that, when *present* (any value), disables all colour.
pub const NO_COLOR_ENV: &str = "NO_COLOR";

/// One of the three built-in themes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeName {
    /// Light, warm — ochre/teal on cream.
    Dawn,
    /// Dark, cool — amber/sky on near-black. The Phase-1 default.
    Night,
    /// Zero override — roles map onto the terminal's own ANSI-16 colours.
    Terminal,
}

impl ThemeName {
    /// The Phase-1 default: `night` (dark terminals are the common case).
    pub const DEFAULT: ThemeName = ThemeName::Night;

    /// Parse a theme name (case-insensitive). `None` for an unrecognized name.
    #[must_use]
    pub fn parse(name: &str) -> Option<ThemeName> {
        match name.trim().to_ascii_lowercase().as_str() {
            "dawn" => Some(ThemeName::Dawn),
            "night" => Some(ThemeName::Night),
            "terminal" => Some(ThemeName::Terminal),
            _ => None,
        }
    }

    /// The lowercase canonical name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeName::Dawn => "dawn",
            ThemeName::Night => "night",
            ThemeName::Terminal => "terminal",
        }
    }
}

/// A semantic colour role. Renderers reference roles, never raw colours, so a
/// theme swap re-skins everything (design §B.2; `bg` is reserved — Phase 1 never
/// paints a background).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Logo, the `›` prompt, focused borders, links.
    Primary,
    /// Headings, selection, slash-palette highlight.
    Accent,
    /// Body text.
    Fg,
    /// Cost line, secondary text, unfocused borders, box frames.
    Dim,
    /// `ok` status, verified receipts, the health dot.
    Success,
    /// 50–85% context pressure, retries.
    Warn,
    /// `err` status, a bad signature, ≥85% context pressure.
    Error,
}

/// A text attribute layered on top of a role's colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attr {
    /// SGR 1 — structural emphasis (headings, the prompt glyph).
    Bold,
    /// SGR 3 — quoted/aside/inline emphasis.
    Italic,
    /// SGR 4 — a live link target (the OSC-8 fallback).
    Underline,
    /// SGR 2 — secondary/metadata.
    Dim,
}

impl Attr {
    fn sgr(self) -> &'static str {
        match self {
            Attr::Bold => "1",
            Attr::Italic => "3",
            Attr::Underline => "4",
            Attr::Dim => "2",
        }
    }
}

/// A single role's foreground colour, as the SGR parameter list that selects it
/// (e.g. `"38;5;179"` for ANSI-256, `"33"` for ANSI-16 yellow). `None` means "no
/// override" — the terminal's default foreground.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Colour(Option<&'static str>);

impl Colour {
    const NONE: Colour = Colour(None);
    const fn sgr(code: &'static str) -> Colour {
        Colour(Some(code))
    }
}

/// The seven painted roles plus the reserved background colour.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Palette {
    primary: Colour,
    accent: Colour,
    fg: Colour,
    dim: Colour,
    success: Colour,
    warn: Colour,
    error: Colour,
}

impl Palette {
    /// `dawn` — light, warm. ANSI-256 mappings from design §B.2.
    const DAWN: Palette = Palette {
        primary: Colour::sgr("38;5;173"), // warm ochre
        accent: Colour::sgr("38;5;30"),   // teal
        fg: Colour::sgr("38;5;235"),
        dim: Colour::sgr("38;5;244"),
        success: Colour::sgr("38;5;28"),
        warn: Colour::sgr("38;5;136"),
        error: Colour::sgr("38;5;160"),
    };

    /// `night` — dark, cool. ANSI-256 mappings from design §B.2.
    const NIGHT: Palette = Palette {
        primary: Colour::sgr("38;5;179"), // amber
        accent: Colour::sgr("38;5;74"),   // sky
        fg: Colour::sgr("38;5;254"),
        dim: Colour::sgr("38;5;242"),
        success: Colour::sgr("38;5;114"),
        warn: Colour::sgr("38;5;179"),
        error: Colour::sgr("38;5;173"),
    };

    /// `terminal` — the terminal's own ANSI-16 palette (design §E).
    const TERMINAL: Palette = Palette {
        primary: Colour::sgr("33"), // yellow
        accent: Colour::sgr("36"),  // cyan
        fg: Colour::NONE,           // default foreground
        dim: Colour::sgr("90"),     // bright-black
        success: Colour::sgr("32"), // green
        warn: Colour::sgr("33"),    // yellow
        error: Colour::sgr("31"),   // red
    };

    const fn for_name(name: ThemeName) -> Palette {
        match name {
            ThemeName::Dawn => Palette::DAWN,
            ThemeName::Night => Palette::NIGHT,
            ThemeName::Terminal => Palette::TERMINAL,
        }
    }

    fn colour(&self, role: Role) -> &Colour {
        match role {
            Role::Primary => &self.primary,
            Role::Accent => &self.accent,
            Role::Fg => &self.fg,
            Role::Dim => &self.dim,
            Role::Success => &self.success,
            Role::Warn => &self.warn,
            Role::Error => &self.error,
        }
    }
}

/// A resolved theme: which palette to paint with, and whether to paint at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    name: ThemeName,
    palette: Palette,
    styled: bool,
}

impl Default for Theme {
    /// The Phase-1 default: a styled `night` theme.
    fn default() -> Self {
        Theme::named(ThemeName::DEFAULT)
    }
}

impl Theme {
    /// A styled theme for `name`.
    #[must_use]
    pub fn named(name: ThemeName) -> Theme {
        Theme {
            name,
            palette: Palette::for_name(name),
            styled: true,
        }
    }

    /// The same theme with styling forced off — every paint becomes the identity.
    /// Used for the `NO_COLOR` / non-tty / `--plain` degradation.
    #[must_use]
    pub fn plain(mut self) -> Theme {
        self.styled = false;
        self
    }

    /// Resolve a theme from an environment lookup: `ARDUR_THEME` selects the
    /// palette (falling back to [`ThemeName::DEFAULT`] when unset or unrecognized),
    /// and the presence of `NO_COLOR` (any value) forces styling off.
    ///
    /// The lookup is injected rather than read from `std::env` directly so callers
    /// can layer tty-detection on top and tests stay hermetic without the
    /// edition-2024 `unsafe` `set_var` (see the module docs).
    pub fn from_lookup<F>(lookup: F) -> Theme
    where
        F: Fn(&str) -> Option<String>,
    {
        let name = lookup(THEME_ENV)
            .as_deref()
            .and_then(ThemeName::parse)
            .unwrap_or(ThemeName::DEFAULT);
        let theme = Theme::named(name);
        if lookup(NO_COLOR_ENV).is_some() {
            theme.plain()
        } else {
            theme
        }
    }

    /// Resolve a theme from the process environment. Equivalent to
    /// [`from_lookup`](Self::from_lookup) over `std::env::var`.
    #[must_use]
    pub fn from_env() -> Theme {
        Theme::from_lookup(|key| std::env::var(key).ok())
    }

    /// The active theme name.
    #[must_use]
    pub fn name(&self) -> ThemeName {
        self.name
    }

    /// Whether this theme emits colour (false under `NO_COLOR`/plain).
    #[must_use]
    pub fn is_styled(&self) -> bool {
        self.styled
    }

    /// Paint `text` in `role`'s colour. The identity when styling is off.
    #[must_use]
    pub fn paint(&self, role: Role, text: &str) -> String {
        self.paint_attr(role, &[], text)
    }

    /// Paint `text` in `role`'s colour with the given attributes layered on. The
    /// identity (returns `text` unchanged) when styling is off.
    #[must_use]
    pub fn paint_attr(&self, role: Role, attrs: &[Attr], text: &str) -> String {
        if !self.styled {
            return text.to_string();
        }
        let mut codes: Vec<&str> = attrs.iter().map(|a| a.sgr()).collect();
        if let Colour(Some(code)) = self.palette.colour(role) {
            codes.push(code);
        }
        if codes.is_empty() {
            return text.to_string();
        }
        format!("\x1b[{}m{text}\x1b[0m", codes.join(";"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips_known_names() {
        for name in [ThemeName::Dawn, ThemeName::Night, ThemeName::Terminal] {
            assert_eq!(ThemeName::parse(name.as_str()), Some(name));
        }
        assert_eq!(ThemeName::parse("DAWN"), Some(ThemeName::Dawn));
        assert_eq!(ThemeName::parse("nope"), None);
    }

    #[test]
    fn from_lookup_reads_theme_and_no_color() {
        let dawn = Theme::from_lookup(|k| (k == THEME_ENV).then(|| "dawn".to_string()));
        assert_eq!(dawn.name(), ThemeName::Dawn);
        assert!(dawn.is_styled());

        let unset = Theme::from_lookup(|_| None);
        assert_eq!(unset.name(), ThemeName::DEFAULT);

        let no_color = Theme::from_lookup(|k| (k == NO_COLOR_ENV).then(String::new));
        assert!(!no_color.is_styled());
    }

    #[test]
    fn paint_is_identity_when_unstyled() {
        let plain = Theme::named(ThemeName::Night).plain();
        assert_eq!(plain.paint(Role::Accent, "hi"), "hi");
    }

    #[test]
    fn paint_wraps_with_sgr_when_styled() {
        let night = Theme::named(ThemeName::Night);
        let out = night.paint_attr(Role::Accent, &[Attr::Bold], "hi");
        assert!(out.starts_with("\x1b[1;38;5;74m"));
        assert!(out.ends_with("\x1b[0m"));
        assert!(out.contains("hi"));
    }
}
