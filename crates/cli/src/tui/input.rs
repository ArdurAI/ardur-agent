use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) const MAX_INPUT_BYTES: usize = 16 * 1024;

#[derive(Clone, Default, Debug)]
pub(super) struct Input {
    pub text: String,
    pub cursor: usize,
}
impl Input {
    pub fn insert(&mut self, text: &str) {
        let clean = super::text::sanitize(text, MAX_INPUT_BYTES);
        let available = MAX_INPUT_BYTES.saturating_sub(self.text.len());
        let mut end = 0;
        for (i, g) in clean.grapheme_indices(true) {
            if i + g.len() > available {
                break;
            }
            end = i + g.len();
        }
        self.text.insert_str(self.cursor, &clean[..end]);
        self.cursor += end;
    }
    pub fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i);
    }
    pub fn right(&mut self) {
        self.cursor += self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(0, str::len);
    }
    pub fn delete(&mut self) {
        let start = self.cursor;
        self.right();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }
    pub fn backspace(&mut self) {
        let old = self.cursor;
        self.left();
        self.text.replace_range(self.cursor..old, "");
    }
    pub fn rows(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(2);
        let mut rows = vec![String::new()];
        let (mut x, mut cursor) = (0, (0, 0));
        for (i, g) in self.text.grapheme_indices(true) {
            let w = g.width();
            if g != "\n" && x + w > width {
                rows.push(String::new());
                x = 0;
            }
            if i == self.cursor {
                cursor = (x, rows.len() - 1);
            }
            if g == "\n" {
                rows.push(String::new());
                x = 0;
            } else {
                rows.last_mut().unwrap().push_str(g);
                x += w;
            }
        }
        if self.cursor == self.text.len() {
            if x >= width {
                rows.push(String::new());
                x = 0;
            }
            cursor = (x, rows.len() - 1);
        }
        (rows, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paste_unicode_editing_and_wrapped_cursor_are_bounded() {
        let mut input = Input::default();
        input.insert("界e\u{301}👩‍💻\r\nlast\x1b[31m\x07");
        assert_eq!(input.text, "界e\u{301}👩‍💻\nlast�[31m�");
        input.text = "界e\u{301}👩‍💻".into();
        input.cursor = input.text.len();
        assert_eq!(
            input.rows(4),
            (vec!["界e\u{301}".into(), "👩‍💻".into()], (2, 1))
        );
        input.left();
        input.backspace();
        assert_eq!(input.text, "界👩‍💻");
        assert_eq!(input.cursor, "界".len());
        input.insert(&"z".repeat(MAX_INPUT_BYTES * 2));
        assert!(input.text.len() <= MAX_INPUT_BYTES);
        assert!(input.text.is_char_boundary(input.cursor));
    }
}
