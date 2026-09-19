use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Convert only renderer-generated SGR, never terminal commands, into cells.
pub(super) fn spans(generated: &str) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    let mut style = Style::default();
    let mut text = String::new();
    let mut chars = generated.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            flush(&mut lines, &mut text, style);
            if chars.next_if_eq(&'[').is_some() {
                let mut code = String::new();
                while chars
                    .peek()
                    .is_some_and(|c| c.is_ascii_digit() || *c == ';')
                {
                    code.push(chars.next().unwrap());
                }
                if chars.next_if_eq(&'m').is_some() {
                    sgr(&code, &mut style);
                }
            }
        } else if c == '\n' {
            flush(&mut lines, &mut text, style);
            lines.push(Line::default());
        } else if !c.is_control() {
            text.push(c);
        }
    }
    flush(&mut lines, &mut text, style);
    lines
}
fn flush(lines: &mut [Line<'static>], text: &mut String, style: Style) {
    if !text.is_empty() {
        lines
            .last_mut()
            .unwrap()
            .spans
            .push(Span::styled(std::mem::take(text), style));
    }
}
fn sgr(code: &str, style: &mut Style) {
    let codes: Vec<u16> = code.split(';').filter_map(|v| v.parse().ok()).collect();
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            7 => *style = style.add_modifier(Modifier::REVERSED),
            9 => *style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            30..=37 => *style = style.fg(Color::Indexed((codes[i] - 30) as u8)),
            90..=97 => *style = style.fg(Color::Indexed((codes[i] - 90 + 8) as u8)),
            39 => *style = style.fg(Color::Reset),
            49 => *style = style.bg(Color::Reset),
            38 | 48 => {
                let background = codes[i] == 48;
                let color = match codes.get(i + 1) {
                    Some(5) if codes.get(i + 2).is_some_and(|n| *n <= 255) => {
                        i += 2;
                        Some(Color::Indexed(codes[i] as u8))
                    }
                    Some(2)
                        if codes
                            .get(i + 2..i + 5)
                            .is_some_and(|rgb| rgb.iter().all(|v| *v <= 255)) =>
                    {
                        i += 4;
                        Some(Color::Rgb(
                            codes[i - 2] as u8,
                            codes[i - 1] as u8,
                            codes[i] as u8,
                        ))
                    }
                    _ => None,
                };
                if let Some(c) = color {
                    *style = if background { style.bg(c) } else { style.fg(c) };
                }
            }
            _ => {}
        }
        i += 1;
    }
}
pub(super) fn role(theme: &crate::Theme, role: crate::Role) -> Style {
    spans(&theme.paint(role, "x"))[0].spans[0].style
}

pub(super) fn wrap(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let width = width.max(2);
    let mut out = vec![];
    for line in lines {
        let mut row = Line::default();
        let mut x = 0;
        for span in line.spans {
            for g in span.content.graphemes(true) {
                if x + g.width() > width {
                    out.push(row);
                    row = Line::default();
                    x = 0;
                }
                row.spans.push(Span::styled(g.to_owned(), span.style));
                x += g.width();
            }
        }
        out.push(row);
    }
    out
}

/// Bound and neutralize terminal controls before any shared renderer sees text.
pub(super) fn sanitize(text: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let c = match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                '\n'
            }
            '\t' => ' ',
            '\n' => '\n',
            c if c.is_control()
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                '�'
            }
            c => c,
        };
        if out.len() + c.len_utf8() > limit {
            break;
        }
        out.push(c);
    }
    out
}
