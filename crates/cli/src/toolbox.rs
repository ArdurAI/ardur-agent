//! §2.X tool-call boxes and the per-turn cost line (design §A.1, §B.2).
//!
//! Both share the rounded-frame language of the markdown code blocks
//! ([`crate::markdown`]) and paint through the active [`Theme`], so the REPL
//! transcript reads as one coherent surface.

use std::time::Duration;

use crate::theme::{Attr, Role, Theme};
use crate::util::{display_width, ellipsize};

/// The maximum width a box/cost line occupies regardless of terminal size
/// (design: boxes cap at 80 columns).
pub const MAX_BOX_COLS: usize = 80;

/// Render a tool-call box (design §A.1):
///
/// ```text
/// ┌─ tool · file.read ─────────────────────┐
/// │ {"path": "src/main.rs"}                 │
/// └─────────────────────────────────────────┘
/// ```
///
/// `args` is the tool's argument summary (JSON or otherwise); each of its lines is
/// width-clamped to the box interior. The frame is dim; the body is normal `fg`.
/// `width` is the column budget (clamp with [`crate::util::layout_width`] /
/// [`MAX_BOX_COLS`] at the call site).
#[must_use]
pub fn render_tool_call_box(name: &str, args: &str, theme: &Theme, width: usize) -> String {
    let width = width.clamp(crate::util::MIN_WIDTH, MAX_BOX_COLS);
    let inner = width.saturating_sub(4).max(4); // "│ " + " │"

    let title = format!(" tool · {name} ");
    let dashes = (inner + 2).saturating_sub(display_width(&title) + 1); // +1 for the lead `─`
    let top = format!("┌─{title}{}┐", "─".repeat(dashes));

    let mut lines = vec![theme.paint(Role::Dim, &top)];
    let body = if args.trim().is_empty() { "{}" } else { args };
    for raw in body.split('\n') {
        let cell = ellipsize(raw.trim_end(), inner);
        let pad = " ".repeat(inner.saturating_sub(display_width(&cell)));
        let left = theme.paint(Role::Dim, "│ ");
        let content = theme.paint(Role::Fg, &cell);
        let right = theme.paint(Role::Dim, " │");
        lines.push(format!("{left}{content}{pad}{right}"));
    }
    lines.push(theme.paint(Role::Dim, &format!("└{}┘", "─".repeat(inner + 2))));
    lines.join("\n")
}

/// The numbers behind a per-turn cost line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TurnStats {
    /// Prompt tokens consumed.
    pub tokens_in: u64,
    /// Completion tokens produced.
    pub tokens_out: u64,
    /// The turn's dollar cost.
    pub cost_dollars: f64,
    /// Wall-clock duration of the turn.
    pub elapsed: Duration,
    /// Fraction of the model's context window used this turn, if known — drives
    /// the context-pressure colour ramp (design §B.2 law #2).
    pub context_frac: Option<f64>,
}

/// Render the dim per-turn cost line, centred between hairline rules to `width`
/// (design §A.1):
///
/// ```text
/// ───── 421 tokens in · 187 out · $0.0023 · 1.4s ─────
/// ```
///
/// The figures take the context-pressure colour: `dim` < 50%, `warn` 50–85%,
/// `error` ≥ 85% (only when `context_frac` is known); the rules stay dim.
#[must_use]
pub fn render_cost_line(stats: &TurnStats, theme: &Theme, width: usize) -> String {
    let width = width.clamp(crate::util::MIN_WIDTH, MAX_BOX_COLS);
    let secs = stats.elapsed.as_secs_f64();
    let text = format!(
        " {} tokens in · {} out · ${:.4} · {:.1}s ",
        stats.tokens_in, stats.tokens_out, stats.cost_dollars, secs
    );
    let role = match stats.context_frac {
        Some(f) if f >= 0.85 => Role::Error,
        Some(f) if f >= 0.50 => Role::Warn,
        _ => Role::Dim,
    };
    let body = theme.paint(role, &text);
    let rule_cols = width.saturating_sub(display_width(&text));
    let left = rule_cols / 2;
    let right = rule_cols - left;
    format!(
        "{}{body}{}",
        theme.paint(Role::Dim, &"─".repeat(left)),
        theme.paint(Role::Dim, &"─".repeat(right)),
    )
}

/// A running tally of a session's spend, for the `/cost` command.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SessionCost {
    /// Number of chat turns billed.
    pub turns: u64,
    /// Cumulative prompt tokens.
    pub tokens_in: u64,
    /// Cumulative completion tokens.
    pub tokens_out: u64,
    /// Cumulative dollar cost.
    pub dollars: f64,
}

impl SessionCost {
    /// Fold one turn's figures into the tally.
    pub fn record(&mut self, tokens_in: u64, tokens_out: u64, dollars: f64) {
        self.turns += 1;
        self.tokens_in += tokens_in;
        self.tokens_out += tokens_out;
        self.dollars += dollars;
    }

    /// Render the `/cost` summary line.
    #[must_use]
    pub fn render(&self, theme: &Theme) -> String {
        let label = theme.paint_attr(Role::Accent, &[Attr::Bold], "session cost");
        let body = theme.paint(
            Role::Dim,
            &format!(
                "{} turns · {} tokens in · {} out · ${:.4}",
                self.turns, self.tokens_in, self.tokens_out, self.dollars
            ),
        );
        format!("{label}  {body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeName;

    fn night() -> Theme {
        Theme::named(ThemeName::Night)
    }

    #[test]
    fn box_width_adapts() {
        for w in [40usize, 60, 80] {
            let out = render_tool_call_box("file.read", "{\"path\":\"a\"}", &night().plain(), w);
            let top = out.lines().next().unwrap();
            assert_eq!(
                display_width(top),
                w,
                "top border should be exactly {w} cols"
            );
            let bottom = out.lines().last().unwrap();
            assert_eq!(display_width(bottom), w);
        }
    }

    #[test]
    fn box_caps_at_max() {
        let out = render_tool_call_box("x", "{}", &night().plain(), 200);
        let top = out.lines().next().unwrap();
        assert_eq!(display_width(top), MAX_BOX_COLS);
    }

    #[test]
    fn cost_line_holds_figures_and_width() {
        let stats = TurnStats {
            tokens_in: 421,
            tokens_out: 187,
            cost_dollars: 0.0023,
            elapsed: Duration::from_millis(1400),
            context_frac: None,
        };
        let out = render_cost_line(&stats, &night().plain(), 60);
        assert!(out.contains("421 tokens in"));
        assert!(out.contains("187 out"));
        assert!(out.contains("$0.0023"));
        assert!(out.contains("1.4s"));
        assert_eq!(display_width(&out), 60);
    }

    #[test]
    fn cost_line_ramps_on_context_pressure() {
        let base = TurnStats {
            tokens_in: 1,
            tokens_out: 1,
            cost_dollars: 0.0,
            elapsed: Duration::from_secs(1),
            context_frac: Some(0.9),
        };
        let out = render_cost_line(&base, &night(), 60);
        // error role = ansi-256 173 for night.
        assert!(out.contains("38;5;173"));
    }
}
