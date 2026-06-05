//! §2.X Phase-1 rendering core — the public-surface acceptance tests
//! (ADR-Phase2-021, design §B–§H).
//!
//! Every check drives the exported rendering API in-process: the Markdown
//! renderer, the tool-call box, the cost line, the typing-dots cadence, theme
//! selection (env var + live `/theme`), the `NO_COLOR` degradation, the
//! first-launch splash, and the Phase-1 slash subset. The environment is injected
//! through `Theme::from_lookup` / explicit paths so the suite stays hermetic — no
//! process-wide `set_var` (which edition 2024 makes `unsafe`).

use std::time::Duration;

use ardur_cli::{
    Attr, Role, SessionCost, TYPING_DOTS_FRAMES, TYPING_DOTS_HZ, TYPING_DOTS_TICK, Theme,
    ThemeName, TurnStats, TypingDots, apply_theme_command, display_width, is_first_launch,
    phase1_help, render_cost_line, render_markdown, render_tool_call_box, show_welcome_if_first,
};

const NO_COLOR_ENV: &str = "NO_COLOR";
const THEME_ENV: &str = "ARDUR_THEME";

fn night() -> Theme {
    Theme::named(ThemeName::Night)
}

#[test]
fn markdown_renders_heading_with_styling() {
    let out = render_markdown("# Findings", &night(), 80);
    // h1 = accent (night sky = 38;5;74) + bold (1) + underline (4).
    assert!(
        out.contains("\x1b[1;4;38;5;74m"),
        "h1 should be bold+underline+accent, got: {out:?}"
    );
    assert!(out.contains("Findings"));
}

#[test]
fn markdown_renders_code_block_with_syntax_highlight() {
    let md = "```rust\npub fn fingerprint() -> String { String::new() }\n```";
    let out = render_markdown(md, &night(), 80);
    // syntect emits ANSI colour escapes for the highlighted tokens...
    assert!(out.contains('\x1b'), "code should be syntax-highlighted");
    // ...inside a framed box with its language label, and the code survives.
    assert!(out.contains('┌') && out.contains('└'));
    assert!(out.contains("rust"));
    assert!(out.contains("fingerprint"));
}

#[test]
fn markdown_renders_table_with_borders() {
    let md = "| Provider | Streaming |\n|----------|-----------|\n| anthropic | yes |";
    let out = render_markdown(md, &night(), 80);
    for glyph in ['┌', '┬', '┐', '│', '├', '┼', '┤', '└', '┴', '┘'] {
        assert!(
            out.contains(glyph),
            "table missing border glyph {glyph}: {out:?}"
        );
    }
    assert!(out.contains("Provider") && out.contains("anthropic"));
}

#[test]
fn tool_call_box_adapts_to_terminal_width() {
    // Each requested width yields a box whose borders are exactly that wide.
    for width in [36usize, 50, 72, 80] {
        let out = render_tool_call_box(
            "file.read",
            "{\"path\":\"src/main.rs\"}",
            &night().plain(),
            width,
        );
        let top = out.lines().next().unwrap();
        let bottom = out.lines().last().unwrap();
        assert_eq!(display_width(top), width, "top border width at {width}");
        assert_eq!(
            display_width(bottom),
            width,
            "bottom border width at {width}"
        );
        assert!(out.contains("tool · file.read"));
    }
}

#[test]
fn cost_line_renders_at_end_of_response() {
    let stats = TurnStats {
        tokens_in: 421,
        tokens_out: 187,
        cost_dollars: 0.0023,
        elapsed: Duration::from_millis(1400),
        context_frac: None,
    };
    let out = render_cost_line(&stats, &night().plain(), 72);
    assert!(out.contains("421 tokens in · 187 out"));
    assert!(out.contains("$0.0023"));
    assert!(out.contains("1.4s"));
    assert_eq!(display_width(&out), 72, "the cost line fills the width");
}

#[test]
fn typing_dots_animate_at_correct_rate() {
    // 4 Hz → a 250 ms tick, three pulsing frames.
    assert_eq!(TYPING_DOTS_HZ, 4);
    assert_eq!(TYPING_DOTS_TICK, Duration::from_millis(250));
    assert_eq!(TYPING_DOTS_FRAMES, ["·", "··", "···"]);

    let mut dots = TypingDots::new();
    let seen: Vec<usize> = (0..4)
        .map(|_| {
            let f = dots.frame();
            dots.tick();
            f
        })
        .collect();
    assert_eq!(seen, vec![0, 1, 2, 0], "frames cycle and wrap");
}

#[test]
fn theme_switching_via_env_var() {
    // `ARDUR_THEME=dawn` selects the dawn palette; an unset var falls to the
    // default (night). Injected through `from_lookup` to stay hermetic.
    let dawn = Theme::from_lookup(|k| (k == THEME_ENV).then(|| "dawn".to_string()));
    assert_eq!(dawn.name(), ThemeName::Dawn);

    let default = Theme::from_lookup(|_| None);
    assert_eq!(default.name(), ThemeName::Night);

    // The dawn primary (ochre = 38;5;173) paints distinctly from night's amber.
    assert!(dawn.paint(Role::Primary, "x").contains("38;5;173"));
}

#[test]
fn theme_switching_via_slash_command() {
    let mut theme = Theme::named(ThemeName::Dawn);
    let msg = apply_theme_command("night", &mut theme).expect("known theme switches");
    assert_eq!(theme.name(), ThemeName::Night);
    assert!(msg.contains("night"));
    // The accent now paints with night's sky (38;5;74), proving the live swap.
    assert!(theme.paint(Role::Accent, "x").contains("38;5;74"));
}

#[test]
fn no_color_env_var_disables_styling() {
    // `NO_COLOR` present (any value) forces an unstyled theme: no escapes anywhere.
    let theme = Theme::from_lookup(|k| (k == NO_COLOR_ENV).then(String::new));
    assert!(!theme.is_styled());

    let md = "# Heading\n\n**bold** and `code`\n\n| a | b |\n|---|---|\n| 1 | 2 |";
    let out = render_markdown(md, &theme, 80);
    assert!(
        !out.contains('\x1b'),
        "NO_COLOR output must be escape-free: {out:?}"
    );
    // Structure (heading text, bullets/borders) survives without colour.
    assert!(out.contains("Heading") && out.contains('┌'));

    // A direct paint is the identity under NO_COLOR.
    assert_eq!(theme.paint_attr(Role::Error, &[Attr::Bold], "x"), "x");
}

#[test]
fn welcome_splash_shows_once_then_persists_state() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("state.toml");
    let theme = night().plain();

    assert!(is_first_launch(&path), "absent state ⇒ first launch");

    let mut first = Vec::new();
    let shown = show_welcome_if_first(&path, &theme, &mut first).expect("splash writes");
    assert!(shown, "the splash shows on first launch");
    let text = String::from_utf8(first).unwrap();
    assert!(text.contains("the agent that keeps the receipts"));
    assert!(path.exists(), "state file is persisted");
    assert!(!is_first_launch(&path), "the bit flipped");

    let mut second = Vec::new();
    let shown_again = show_welcome_if_first(&path, &theme, &mut second).expect("second run");
    assert!(!shown_again, "the splash never shows again");
    assert!(second.is_empty(), "no output on a returning launch");
}

#[test]
fn slash_help_lists_phase1_commands() {
    let help = phase1_help();
    for cmd in ["/help", "/clear", "/theme", "/cost", "/quit", "/exit"] {
        assert!(help.contains(cmd), "help should list {cmd}, got: {help}");
    }
    // Phase-2/3 commands are deferred — they must NOT appear yet.
    for deferred in [
        "/sessions",
        "/receipts",
        "/memory",
        "/skill",
        "/copy",
        "/save",
    ] {
        assert!(
            !help.contains(deferred),
            "{deferred} is deferred past Phase 1"
        );
    }
}

#[test]
fn slash_theme_unknown_returns_error() {
    let mut theme = Theme::named(ThemeName::Night);
    let err = apply_theme_command("ultraviolet", &mut theme).unwrap_err();
    assert!(
        err.contains("ultraviolet"),
        "error names the bad theme: {err}"
    );
    // A failed switch leaves the active theme untouched.
    assert_eq!(theme.name(), ThemeName::Night);
}

/// A running cost tally folds turns and renders the `/cost` summary.
#[test]
fn session_cost_tally_accumulates() {
    let mut cost = SessionCost::default();
    cost.record(100, 50, 0.0021);
    cost.record(200, 80, 0.0040);
    let out = cost.render(&night().plain());
    assert!(out.contains("2 turns"));
    assert!(out.contains("300 tokens in · 130 out"));
    assert!(out.contains("$0.0061"));
}
