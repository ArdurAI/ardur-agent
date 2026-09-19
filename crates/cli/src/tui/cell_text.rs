use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Cell-width adapter for the shared renderer's legacy scalar-width layout.
/// Non-ASCII graphemes become one opaque scalar per terminal cell during
/// layout, then are restored in the styled output. CommonMark and highlighting
/// still run only in the shared renderer; its REPL bytes do not change. A
/// scalar-width ellipsis that cuts a wide token cannot emit half a grapheme.
#[derive(Default)]
pub(super) struct CellText {
    graphemes: Vec<(String, usize)>,
}
impl CellText {
    const BASE: u32 = 0xf0000;
    pub fn encode(&mut self, text: &str) -> String {
        let mut out = String::new();
        for g in text.graphemes(true) {
            if g.is_ascii() {
                out.push_str(g);
                continue;
            }
            // Inputs are bounded to 32 KiB: fewer non-ASCII graphemes than
            // slots in this private-use range, including a tool's name.
            let token =
                char::from_u32(Self::BASE + self.graphemes.len() as u32).expect("bounded block");
            let width = g.width().max(1);
            self.graphemes.push((
                if g.width() == 0 {
                    format!("◌{g}")
                } else {
                    g.to_owned()
                },
                width,
            ));
            out.extend(std::iter::repeat_n(token, width));
        }
        out
    }
    pub fn decode(&self, generated: &str) -> Vec<Line<'static>> {
        let mut lines = super::text::spans(generated);
        for line in &mut lines {
            for span in &mut line.spans {
                let mut out = String::new();
                let mut chars = span.content.chars().peekable();
                while let Some(c) = chars.next() {
                    let entry = (c as u32)
                        .checked_sub(Self::BASE)
                        .and_then(|i| self.graphemes.get(i as usize));
                    if let Some((g, width)) = entry {
                        let mut count = 1;
                        while chars.peek() == Some(&c) {
                            chars.next();
                            count += 1;
                        }
                        if count == *width {
                            out.push_str(g);
                        } else {
                            out.extend(std::iter::repeat_n('…', count));
                        }
                    } else {
                        out.push(c);
                    }
                }
                span.content = out.into();
            }
        }
        lines
    }
}
