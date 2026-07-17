//! §2.X inline Markdown rendering for the stunning-CLI core (design §C,
//! ADR-Phase2-021).
//!
//! [`render_markdown`] parses CommonMark (GitHub tables enabled) with `comrak`,
//! then walks the AST emitting *our own* themed terminal spans — never an
//! HTML-to-ANSI bridge — so every glyph, box, and colour is under our control and
//! shared with the tool boxes ([`crate::toolbox`]). Supported elements:
//!
//! - **Headings** h1 (accent · bold · underline), h2/h3 (accent · bold · dim);
//!   h4+ degrade to h3.
//! - **Lists** — `•`/`◦`/`▪` by depth (bullets) or the number (ordered), in the
//!   primary colour, indent preserved.
//! - **Inline** — **bold**, *italic*, `code` (accent), links as `text (url)` or an
//!   OSC-8 hyperlink (the [`with_links`](render_markdown_with) variant).
//! - **Code blocks** — syntect syntax highlight (rust/python/ts/sh/json/yaml/toml
//!   and the rest of the bundled grammars) inside a dim rounded frame with a
//!   language label.
//! - **Tables** — box-drawn (`┌─┬─┐ │ └─┴─┘`), header row accent-bold, auto-width
//!   capped to the terminal.
//! - **Block quotes** — a dim `▏ ` bar, text italic.
//! - **Thematic breaks** — a dim hairline rule.
//!
//! Under an unstyled [`Theme`] (`NO_COLOR`/non-tty/`--plain`) the same structure
//! renders with no escapes: boxes and bullets remain, colour drops out.

use std::sync::OnceLock;

use comrak::nodes::{AstNode, ListType, NodeValue};
use comrak::{Arena, Options, parse_document};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme as SynTheme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::as_24_bit_terminal_escaped;

use crate::theme::{Attr, Role, Theme};
use crate::util::{display_width, ellipsize};

/// How inline links are rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkStyle {
    /// `text (url)` — the URL is always visible. The safe universal default.
    Parenthetical,
    /// An OSC-8 hyperlink — `text` underlined and clickable, URL hidden in the
    /// escape. Emitted only where the terminal advertises OSC-8 support.
    Osc8,
}

/// Render `src` (CommonMark) to themed terminal text laid out for `width`
/// columns, with links shown as `text (url)`.
#[must_use]
pub fn render_markdown(src: &str, theme: &Theme, width: usize) -> String {
    render_doc(src, theme, width, LinkStyle::Parenthetical)
}

/// [`render_markdown`] with explicit control over OSC-8 hyperlink emission. Pass
/// `osc8 = true` only when the terminal supports OSC-8 (see
/// [`crate::links::terminal_supports_osc8`]); otherwise links degrade to
/// `text (url)`.
#[must_use]
pub fn render_markdown_with(src: &str, theme: &Theme, width: usize, osc8: bool) -> String {
    let style = if osc8 {
        LinkStyle::Osc8
    } else {
        LinkStyle::Parenthetical
    };
    render_doc(src, theme, width, style)
}

fn render_doc(src: &str, theme: &Theme, width: usize, links: LinkStyle) -> String {
    let arena = Arena::new();
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    let root = parse_document(&arena, src, &options);
    let ctx = Ctx {
        theme,
        width: width.max(crate::util::MIN_WIDTH),
        links,
    };
    let mut out = String::new();
    render_blocks(root, &ctx, 0, &mut out);
    // A single trailing newline, no more — callers print this as one block.
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Immutable rendering context threaded through the walk.
struct Ctx<'a> {
    theme: &'a Theme,
    width: usize,
    links: LinkStyle,
}

/// Render the block-level children of `parent`, appending lines to `out`. `indent`
/// is the current left-margin in spaces (lists nest it).
fn render_blocks<'a>(parent: &'a AstNode<'a>, ctx: &Ctx, indent: usize, out: &mut String) {
    for node in parent.children() {
        render_block(node, ctx, indent, out);
    }
}

fn render_block<'a>(node: &'a AstNode<'a>, ctx: &Ctx, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match &node.data.borrow().value {
        NodeValue::Heading(h) => {
            let text = inline_plain(node);
            push_line(out, &pad, &render_heading(&text, h.level, ctx));
        }
        NodeValue::Paragraph => {
            let line = render_inline(node, ctx, Role::Fg);
            push_line(out, &pad, &line);
        }
        NodeValue::List(_) => {
            render_list(node, ctx, indent, out);
        }
        NodeValue::CodeBlock(cb) => {
            // One block: the box lines share a single trailing separator (no blank
            // line between rows).
            push_line(
                out,
                &pad,
                &render_code_block(&cb.info, &cb.literal, ctx).join("\n"),
            );
        }
        NodeValue::BlockQuote => {
            let mut inner = String::new();
            render_blocks(node, ctx, 0, &mut inner);
            let bar = ctx.theme.paint(Role::Dim, "▏ ");
            let quoted: Vec<String> = inner
                .trim_end()
                .lines()
                // Re-paint each quoted line uniformly dim+italic (design §C): strip
                // the inner span colours so the quote reads as a single aside.
                .map(crate::util::strip_ansi)
                .filter(|l| !l.is_empty())
                .map(|l| {
                    format!(
                        "{bar}{}",
                        ctx.theme.paint_attr(Role::Dim, &[Attr::Italic], &l)
                    )
                })
                .collect();
            if !quoted.is_empty() {
                push_line(out, &pad, &quoted.join("\n"));
            }
        }
        NodeValue::Table(_) => {
            push_line(out, &pad, &render_table(node, ctx).join("\n"));
        }
        NodeValue::ThematicBreak => {
            let rule = "─".repeat(ctx.width.saturating_sub(indent));
            push_line(out, &pad, &ctx.theme.paint(Role::Dim, &rule));
        }
        // Anything else (HTML blocks, footnotes, …): fall back to its inline text.
        _ => {
            let line = render_inline(node, ctx, Role::Fg);
            if !line.is_empty() {
                push_line(out, &pad, &line);
            }
        }
    }
}

/// Push one logical line (which may itself contain `\n`) at `pad`, then a blank
/// separator line, so blocks breathe.
fn push_line(out: &mut String, pad: &str, content: &str) {
    for line in content.split('\n') {
        out.push_str(pad);
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
}

fn render_heading(text: &str, level: u8, ctx: &Ctx) -> String {
    match level {
        1 => ctx
            .theme
            .paint_attr(Role::Accent, &[Attr::Bold, Attr::Underline], text),
        2 => {
            // h2 — bold dim-accent with a hairline rule out to the margin.
            let styled = ctx
                .theme
                .paint_attr(Role::Accent, &[Attr::Bold, Attr::Dim], text);
            let used = display_width(text) + 4; // "── " + " "
            let trail = ctx.width.saturating_sub(used);
            let rule = ctx.theme.paint(Role::Dim, &"─".repeat(trail));
            let lead = ctx.theme.paint(Role::Dim, "── ");
            format!("{lead}{styled} {rule}")
        }
        _ => ctx
            .theme
            .paint_attr(Role::Accent, &[Attr::Bold, Attr::Dim], text),
    }
}

fn render_list<'a>(list: &'a AstNode<'a>, ctx: &Ctx, indent: usize, out: &mut String) {
    let (list_type, start) = match &list.data.borrow().value {
        NodeValue::List(nl) => (nl.list_type, nl.start),
        _ => return,
    };
    let depth = indent / 2;
    for (i, item) in list.children().enumerate() {
        let marker = match list_type {
            ListType::Ordered => format!("{}.", start + i),
            ListType::Bullet => match depth {
                0 => "•".to_string(),
                1 => "◦".to_string(),
                _ => "▪".to_string(),
            },
        };
        let marker = ctx.theme.paint(Role::Primary, &marker);
        // The item's first paragraph sits on the marker line; nested blocks follow.
        let mut first = true;
        for child in item.children() {
            match &child.data.borrow().value {
                NodeValue::Paragraph if first => {
                    let text = render_inline(child, ctx, Role::Fg);
                    out.push_str(&" ".repeat(indent));
                    out.push_str(&format!("{marker} {text}\n"));
                    first = false;
                }
                NodeValue::List(_) => render_list(child, ctx, indent + 2, out),
                _ => {
                    let mut inner = String::new();
                    render_block(child, ctx, indent + 2, &mut inner);
                    out.push_str(inner.trim_end_matches('\n'));
                    out.push('\n');
                    first = false;
                }
            }
        }
    }
    out.push('\n');
}

// ---- inline -------------------------------------------------------------------

/// Concatenate the visible text of an inline subtree, ignoring emphasis — used for
/// headings, link labels, and table-cell width measurement.
fn inline_plain<'a>(node: &'a AstNode<'a>) -> String {
    let mut s = String::new();
    collect_text(node, &mut s);
    s
}

fn collect_text<'a>(node: &'a AstNode<'a>, out: &mut String) {
    match &node.data.borrow().value {
        NodeValue::Text(t) => out.push_str(t),
        NodeValue::Code(c) => out.push_str(&c.literal),
        NodeValue::SoftBreak | NodeValue::LineBreak => out.push(' '),
        _ => {
            for child in node.children() {
                collect_text(child, out);
            }
        }
    }
}

/// Render the inline children of `parent` as one styled string, with `base` as the
/// surrounding text colour.
fn render_inline<'a>(parent: &'a AstNode<'a>, ctx: &Ctx, base: Role) -> String {
    let mut out = String::new();
    for node in parent.children() {
        match &node.data.borrow().value {
            NodeValue::Text(t) => out.push_str(&ctx.theme.paint(base, t)),
            NodeValue::Code(c) => out.push_str(&ctx.theme.paint(Role::Accent, &c.literal)),
            NodeValue::Strong => {
                out.push_str(
                    &ctx.theme
                        .paint_attr(base, &[Attr::Bold], &inline_plain(node)),
                );
            }
            NodeValue::Emph | NodeValue::Strikethrough => {
                out.push_str(
                    &ctx.theme
                        .paint_attr(base, &[Attr::Italic], &inline_plain(node)),
                );
            }
            NodeValue::SoftBreak => out.push(' '),
            NodeValue::LineBreak => out.push('\n'),
            NodeValue::Link(l) => out.push_str(&render_link(node, &l.url, ctx, base)),
            NodeValue::Image(l) => {
                let label = inline_plain(node);
                out.push_str(&ctx.theme.paint(base, &label));
                out.push_str(&ctx.theme.paint(Role::Dim, &format!(" ({})", l.url)));
            }
            _ => out.push_str(&render_inline(node, ctx, base)),
        }
    }
    out
}

fn render_link<'a>(node: &'a AstNode<'a>, url: &str, ctx: &Ctx, base: Role) -> String {
    let label = inline_plain(node);
    match ctx.links {
        LinkStyle::Osc8 => {
            let shown = ctx
                .theme
                .paint_attr(Role::Primary, &[Attr::Underline], &label);
            format!("\x1b]8;;{url}\x1b\\{shown}\x1b]8;;\x1b\\")
        }
        LinkStyle::Parenthetical => {
            let text = ctx.theme.paint(base, &label);
            let url = ctx.theme.paint(Role::Dim, &format!(" ({url})"));
            format!("{text}{url}")
        }
    }
}

// ---- code blocks --------------------------------------------------------------

/// Lazily-built syntect syntax + theme sets (loading the bundled dumps costs tens
/// of ms, so it happens once per process).
fn syntaxes() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn syn_theme(theme: &Theme) -> &'static SynTheme {
    static LIGHT: OnceLock<SynTheme> = OnceLock::new();
    static DARK: OnceLock<SynTheme> = OnceLock::new();
    let want_light = matches!(theme.name(), crate::theme::ThemeName::Dawn);
    if want_light {
        LIGHT.get_or_init(|| {
            let ts = ThemeSet::load_defaults();
            ts.themes["InspiredGitHub"].clone()
        })
    } else {
        DARK.get_or_init(|| {
            let ts = ThemeSet::load_defaults();
            ts.themes["base16-ocean.dark"].clone()
        })
    }
}

/// Render a fenced code block: a dim rounded frame with the language label on the
/// top border, syntect-highlighted body (when styled). Returns the box lines.
fn render_code_block(info: &str, code: &str, ctx: &Ctx) -> Vec<String> {
    let lang = info.split_whitespace().next().unwrap_or("").trim();
    let ss = syntaxes();
    let syntax = (!lang.is_empty())
        .then(|| ss.find_syntax_by_token(lang))
        .flatten();
    let label = if lang.is_empty() { "text" } else { lang };

    let inner = ctx.width.saturating_sub(4).max(4); // "│ " + " │"
    let mut lines = Vec::new();
    lines.push(box_top(label, inner, ctx));

    let body = code.strip_suffix('\n').unwrap_or(code);
    let mut highlighter = match (ctx.theme.is_styled(), syntax) {
        (true, Some(syn)) => Some(HighlightLines::new(syn, syn_theme(ctx.theme))),
        _ => None,
    };
    for raw in body.split('\n') {
        let (content, vis) = match highlighter.as_mut() {
            Some(h) => {
                let ranges = h.highlight_line(raw, ss).unwrap_or_default();
                let painted = as_24_bit_terminal_escaped(&ranges, false);
                (format!("{painted}\x1b[0m"), display_width(raw))
            }
            None => (raw.to_string(), raw.chars().count()),
        };
        lines.push(box_body(&content, vis, inner, ctx));
    }
    lines.push(box_bottom(inner, ctx));
    lines
}

fn box_top(label: &str, inner: usize, ctx: &Ctx) -> String {
    // The top border spans the same width as the body: `┌` + (inner + 2) + `┐`.
    let lbl = format!(" {label} ");
    let dashes = (inner + 2).saturating_sub(display_width(&lbl));
    let bar = format!("┌{lbl}{}┐", "─".repeat(dashes));
    ctx.theme.paint(Role::Dim, &bar)
}

fn box_body(content: &str, vis: usize, inner: usize, ctx: &Ctx) -> String {
    let left = ctx.theme.paint(Role::Dim, "│ ");
    if vis <= inner {
        let pad = " ".repeat(inner - vis);
        let right = ctx.theme.paint(Role::Dim, " │");
        format!("{left}{content}{pad}{right}")
    } else {
        // Content wider than the frame: keep it, drop the right border (never cut
        // mid-escape).
        format!("{left}{content}")
    }
}

fn box_bottom(inner: usize, ctx: &Ctx) -> String {
    ctx.theme
        .paint(Role::Dim, &format!("└{}┘", "─".repeat(inner + 2)))
}

// ---- tables -------------------------------------------------------------------

fn render_table<'a>(table: &'a AstNode<'a>, ctx: &Ctx) -> Vec<String> {
    // Collect rows as plain-text cells (for width) plus whether each is the header.
    let mut rows: Vec<(bool, Vec<String>)> = Vec::new();
    for row in table.children() {
        let is_header = matches!(row.data.borrow().value, NodeValue::TableRow(true));
        let mut cells = Vec::new();
        for cell in row.children() {
            cells.push(inline_plain(cell).trim().to_string());
        }
        rows.push((is_header, cells));
    }
    if rows.is_empty() {
        return Vec::new();
    }
    let cols = rows.iter().map(|(_, c)| c.len()).max().unwrap_or(0);
    if cols == 0 {
        return Vec::new();
    }

    // Natural column widths, then shrink the widest until the table fits `width`.
    let mut widths = vec![0usize; cols];
    for (_, cells) in &rows {
        for (i, cell) in cells.iter().enumerate() {
            widths[i] = widths[i].max(display_width(cell));
        }
    }
    let frame = |w: &[usize]| w.iter().sum::<usize>() + 3 * cols + 1;
    while frame(&widths) > ctx.width {
        let Some((idx, _)) = widths.iter().enumerate().max_by_key(|&(_, w)| *w) else {
            break;
        };
        if widths[idx] <= 3 {
            break;
        }
        widths[idx] -= 1;
    }

    let mut out = Vec::new();
    out.push(rule_row(&widths, '┌', '┬', '┐', ctx));
    for (ri, (is_header, cells)) in rows.iter().enumerate() {
        let mut line = ctx.theme.paint(Role::Dim, "│");
        for (i, &w) in widths.iter().enumerate() {
            let raw = cells.get(i).cloned().unwrap_or_default();
            let cell = ellipsize(&raw, w);
            let pad = " ".repeat(w.saturating_sub(display_width(&cell)));
            let styled = if *is_header {
                ctx.theme.paint_attr(Role::Accent, &[Attr::Bold], &cell)
            } else {
                ctx.theme.paint(Role::Fg, &cell)
            };
            line.push_str(&format!(" {styled}{pad} "));
            line.push_str(&ctx.theme.paint(Role::Dim, "│"));
        }
        out.push(line);
        if *is_header && ri == 0 {
            out.push(rule_row(&widths, '├', '┼', '┤', ctx));
        }
    }
    out.push(rule_row(&widths, '└', '┴', '┘', ctx));
    out
}

fn rule_row(widths: &[usize], left: char, mid: char, right: char, ctx: &Ctx) -> String {
    let mut s = String::new();
    s.push(left);
    for (i, w) in widths.iter().enumerate() {
        s.push_str(&"─".repeat(w + 2));
        s.push(if i + 1 == widths.len() { right } else { mid });
    }
    ctx.theme.paint(Role::Dim, &s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeName;

    fn night() -> Theme {
        Theme::named(ThemeName::Night)
    }

    #[test]
    fn heading_h1_is_bold_underline_accent() {
        let out = render_markdown("# Title", &night(), 80);
        // accent (sky 74) + bold (1) + underline (4)
        assert!(out.contains("\x1b[1;4;38;5;74m"));
        assert!(out.contains("Title"));
    }

    #[test]
    fn code_block_is_highlighted() {
        let md = "```rust\nfn main() {}\n```";
        let out = render_markdown(md, &night(), 80);
        assert!(out.contains("\x1b[")); // syntect escapes present
        assert!(out.contains("fn"));
        assert!(out.contains("┌") && out.contains("└"));
        assert!(out.contains("rust"));
    }

    #[test]
    fn table_has_box_borders() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let out = render_markdown(md, &night(), 80);
        for g in ['┌', '┬', '┐', '│', '├', '┼', '┤', '└', '┴', '┘'] {
            assert!(out.contains(g), "missing {g}");
        }
        assert!(out.contains('A') && out.contains('B'));
    }

    #[test]
    fn unstyled_emits_no_escapes_but_keeps_structure() {
        let plain = night().plain();
        let md = "# H\n\n- one\n- two\n\n`code`";
        let out = render_markdown(md, &plain, 80);
        assert!(!out.contains('\x1b'));
        assert!(out.contains('•'));
        assert!(out.contains('H'));
    }

    #[test]
    fn links_parenthetical_then_osc8() {
        let md = "[spec](https://ardur.dev)";
        let paren = render_markdown(md, &night(), 80);
        assert!(paren.contains("https://ardur.dev"));
        let osc8 = render_markdown_with(md, &night(), 80, true);
        assert!(osc8.contains("\x1b]8;;https://ardur.dev"));
    }
}
