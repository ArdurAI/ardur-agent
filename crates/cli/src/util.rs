//! Width helpers shared by the §2.X rendering core.
//!
//! Terminal layout (boxes, tables, rules) needs the *visible* width of a string —
//! the cell count a terminal advances — which excludes ANSI SGR escapes. Phase 1
//! approximates a cell as one `char`; the box-drawing glyphs, bullets, and prose
//! the renderers emit are all single-width, so this is exact for our output
//! without pulling a Unicode-width table (a Phase-2 refinement if wide CJK/emoji
//! content appears).

/// The terminal width to lay out against: the real terminal columns when stdout
/// is a tty, else `COLUMNS`, else a conventional 80. Capped to `max_cols` (the
/// design caps boxes at 80 regardless of a very wide terminal).
#[must_use]
pub fn layout_width(max_cols: usize) -> usize {
    let detected = crossterm::terminal::size()
        .ok()
        .map(|(cols, _)| cols as usize)
        .filter(|&c| c > 0)
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .filter(|&c| c > 0)
        })
        .unwrap_or(80);
    detected.min(max_cols).max(MIN_WIDTH)
}

/// The narrowest layout the renderers will target — below this, boxes stop making
/// sense, so everything clamps up to it.
pub const MIN_WIDTH: usize = 20;

/// The visible width of `s`: its `char` count with ANSI SGR (`ESC [ … m`) and
/// OSC-8 hyperlink (`ESC ] 8 ; … ST`) escapes removed.
#[must_use]
pub fn display_width(s: &str) -> usize {
    strip_ansi(s).chars().count()
}

/// `s` with ANSI SGR and OSC-8 escape sequences removed — the visible text only.
#[must_use]
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI `ESC [ … <final>` — consume up to and including the final byte.
            Some('[') => {
                chars.next();
                for d in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&d) {
                        break;
                    }
                }
            }
            // OSC `ESC ] … (BEL | ESC \)` — consume to the terminator.
            Some(']') => {
                chars.next();
                while let Some(d) = chars.next() {
                    if d == '\x07' {
                        break;
                    }
                    if d == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Truncate `s` (assumed escape-free) to at most `max` visible columns, marking a
/// truncation with a trailing `…` (so the result is ≤ `max`). `max == 0` yields an
/// empty string.
#[must_use]
pub fn ellipsize(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let keep = max - 1;
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_sgr_and_osc8() {
        assert_eq!(strip_ansi("\x1b[1;38;5;74mhi\x1b[0m"), "hi");
        assert_eq!(strip_ansi("\x1b]8;;https://x\x07link\x1b]8;;\x07"), "link");
        assert_eq!(display_width("\x1b[2mab\x1b[0m"), 2);
    }

    #[test]
    fn ellipsize_caps_visible_width() {
        assert_eq!(ellipsize("hello", 10), "hello");
        assert_eq!(ellipsize("hello", 3), "he…");
        assert_eq!(display_width(&ellipsize("hello world", 5)), 5);
    }
}
