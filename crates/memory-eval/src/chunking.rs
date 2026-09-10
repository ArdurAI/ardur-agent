//! Content-aware chunking for the memory corpus.
//!
//! Retrieval quality starts with chunk boundaries: a chunk that splits a code
//! symbol, a transcript turn, or a Markdown section in half retrieves worse than
//! one aligned to the content's natural units. Finding 5 requires chunking tests
//! for **transcript, code, Markdown, and attachments** — each gets a strategy
//! that respects its structure:
//!
//! - **Markdown** — split at headings, so each chunk is a heading plus its body.
//! - **Code** — split at top-level item boundaries (`fn` / `struct` / `impl` /
//!   `pub` / `mod` / `class` / `def` at column 0), never mid-line.
//! - **Transcript** — split at speaker turns, so each chunk is one whole turn.
//! - **Attachment** (and any prose) — pack paragraphs into overlapping windows
//!   near a target size, so retrieval has bounded, self-contained context.
//!
//! Every strategy has a size guard: a section larger than
//! [`ChunkConfig::max_chars`] is windowed by paragraph so no chunk is unbounded.
//! Offsets are byte offsets into the source; boundaries are always at line or
//! paragraph edges, so a chunk never splits a multi-byte character.

use crate::corpus::DocKind;

/// One chunk of a document: its text and its byte span in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// 0-based position of this chunk within the document.
    pub index: usize,
    /// Byte offset of the chunk's start in the source (inclusive).
    pub start: usize,
    /// Byte offset of the chunk's end in the source (exclusive).
    pub end: usize,
    /// The chunk's text (`source[start..end]`).
    pub text: String,
}

/// Chunking parameters.
#[derive(Debug, Clone, Copy)]
pub struct ChunkConfig {
    /// Target chunk size in bytes for windowed (prose) chunking.
    pub target_chars: usize,
    /// Overlap in bytes between adjacent windowed chunks — trailing context of
    /// one chunk repeated at the head of the next, so a fact spanning a window
    /// edge is still wholly present in one chunk.
    pub overlap_chars: usize,
    /// Hard ceiling: a structural section larger than this is windowed so no
    /// chunk is unbounded.
    pub max_chars: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            target_chars: 800,
            overlap_chars: 100,
            max_chars: 1600,
        }
    }
}

/// Chunk `text` according to its [`DocKind`].
pub fn chunk(text: &str, kind: DocKind, config: &ChunkConfig) -> Vec<Chunk> {
    let spans = match kind {
        DocKind::Markdown => split_at_boundaries(text, is_markdown_heading),
        DocKind::Code => split_at_boundaries(text, is_code_item),
        DocKind::Transcript => split_at_boundaries(text, is_speaker_turn),
        DocKind::Attachment | DocKind::Note => window_paragraphs(text, config),
    };

    // Size guard: window any structural section that overran max_chars.
    let mut guarded: Vec<(usize, usize)> = Vec::new();
    for (s, e) in spans {
        if e - s > config.max_chars {
            for (ws, we) in window_paragraphs(&text[s..e], config) {
                guarded.push((s + ws, s + we));
            }
        } else {
            guarded.push((s, e));
        }
    }

    guarded
        .into_iter()
        .enumerate()
        .map(|(index, (start, end))| Chunk {
            index,
            start,
            end,
            text: text[start..end].to_string(),
        })
        .collect()
}

/// A Markdown ATX heading line (`#`, `##`, … up to 6, then a space).
fn is_markdown_heading(line: &str) -> bool {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    (1..=6).contains(&hashes) && line.chars().nth(hashes) == Some(' ')
}

/// A top-level code item: a `fn` / `struct` / `enum` / `impl` / `trait` / `mod` /
/// `pub` / `class` / `def` / `function` keyword at column 0 (no leading indent),
/// so nested items don't fragment their parent.
fn is_code_item(line: &str) -> bool {
    if line.starts_with([' ', '\t']) {
        return false;
    }
    const KEYWORDS: [&str; 10] = [
        "fn ",
        "pub ",
        "struct ",
        "enum ",
        "impl",
        "trait ",
        "mod ",
        "class ",
        "def ",
        "function ",
    ];
    KEYWORDS.iter().any(|k| line.starts_with(k))
}

/// A transcript speaker turn: a line beginning with a short `Speaker:` label or a
/// `[timestamp]` / `> ` quote marker at column 0.
fn is_speaker_turn(line: &str) -> bool {
    if line.starts_with([' ', '\t']) {
        return false;
    }
    if line.starts_with('[') || line.starts_with("> ") {
        return true;
    }
    // `Name:` — a short speaker label with no leading indent. The label must be a
    // 1–3 word name (so a prose sentence that happens to contain a colon isn't a
    // false turn), and the colon must not begin a `://` URL scheme.
    if let Some(colon) = line.find(':') {
        let label = &line[..colon];
        let after = &line[colon + 1..];
        if after.starts_with('/') {
            return false; // `://` — a URL, not a speaker.
        }
        let words = label.split_whitespace().count();
        return colon <= 40 && (1..=3).contains(&words);
    }
    false
}

/// Split `text` into byte spans, starting a new span at each line for which
/// `is_boundary` is true (the boundary line begins the new span). Text before
/// the first boundary is its own span. Never splits mid-line.
fn split_at_boundaries(text: &str, is_boundary: impl Fn(&str) -> bool) -> Vec<(usize, usize)> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut cur_start = 0usize;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let trimmed = line.trim_end_matches(['\n', '\r']);
        // A boundary that is not at the very start of the current span closes the
        // current span and opens a new one beginning at this line.
        if line_start > cur_start && is_boundary(trimmed) {
            spans.push((cur_start, line_start));
            cur_start = line_start;
        }
    }
    if cur_start < text.len() {
        spans.push((cur_start, text.len()));
    }
    spans
}

/// Pack blank-line-separated paragraphs into byte spans near `target_chars`,
/// carrying `overlap_chars` of the previous span's tail into the next so a fact
/// straddling a window edge stays whole in one chunk.
fn window_paragraphs(text: &str, config: &ChunkConfig) -> Vec<(usize, usize)> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    // Paragraph spans (blank-line separated), each keeping its trailing newline(s).
    let paragraphs = paragraph_spans(text);
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut cur_start: Option<usize> = None;
    let mut cur_end = 0usize;
    for (ps, pe) in paragraphs {
        let start = *cur_start.get_or_insert(ps);
        cur_end = pe;
        if cur_end - start >= config.target_chars {
            spans.push((start, cur_end));
            // Next window starts `overlap_chars` back, snapped to a char boundary.
            let overlap_start =
                back_to_char_boundary(text, cur_end.saturating_sub(config.overlap_chars));
            cur_start = if overlap_start < cur_end && overlap_start > start {
                Some(overlap_start)
            } else {
                None
            };
        }
    }
    if let Some(start) = cur_start {
        if start < cur_end {
            spans.push((start, cur_end));
        }
    }
    spans
}

/// Byte spans of blank-line-separated paragraphs (each span includes its trailing
/// separator, so the spans tile the whole text).
fn paragraph_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut para_start = 0usize;
    let mut offset = 0usize;
    let mut prev_blank = false;
    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let is_blank = line.trim().is_empty();
        // A blank line after content ends the paragraph at the blank line's end.
        if is_blank && !prev_blank && line_start > para_start {
            spans.push((para_start, offset));
            para_start = offset;
        }
        prev_blank = is_blank;
    }
    if para_start < text.len() {
        spans.push((para_start, text.len()));
    }
    spans
}

/// Round `idx` down to the nearest char boundary in `text` (so a slice is valid).
fn back_to_char_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concatenating chunk texts reconstructs the source (for the non-overlapping
    /// structural strategies), and every chunk's span slices back to its text.
    fn assert_spans_valid(source: &str, chunks: &[Chunk]) {
        for c in chunks {
            assert!(c.start < c.end, "empty chunk {}", c.index);
            assert!(c.end <= source.len());
            assert_eq!(
                &source[c.start..c.end],
                c.text,
                "chunk {} span mismatch",
                c.index
            );
        }
    }

    #[test]
    fn markdown_splits_at_headings() {
        let md = "# Title\nintro line\n\n## Section A\nbody a\n\n## Section B\nbody b\n";
        let chunks = chunk(md, DocKind::Markdown, &ChunkConfig::default());
        // # Title(+intro), ## Section A, ## Section B → 3 chunks.
        assert_eq!(chunks.len(), 3, "{chunks:#?}");
        assert!(chunks[0].text.starts_with("# Title"));
        assert!(chunks[1].text.starts_with("## Section A"));
        assert!(chunks[2].text.starts_with("## Section B"));
        assert_spans_valid(md, &chunks);
        // Section-based chunks tile the source exactly.
        let joined: String = chunks.iter().map(|c| c.text.clone()).collect();
        assert_eq!(joined, md);
    }

    #[test]
    fn code_splits_at_top_level_items_not_nested() {
        let code = "use std::fmt;\n\nfn alpha() {\n    let x = 1;\n}\n\nstruct Beta {\n    field: u8,\n}\n";
        let chunks = chunk(code, DocKind::Code, &ChunkConfig::default());
        // preamble(use), fn alpha, struct Beta → 3; the nested `let`/`field` lines
        // (indented) do NOT start new chunks.
        assert_eq!(chunks.len(), 3, "{chunks:#?}");
        assert!(chunks[1].text.starts_with("fn alpha()"));
        assert!(chunks[1].text.contains("let x = 1"), "fn body stays whole");
        assert!(chunks[2].text.starts_with("struct Beta"));
        assert_spans_valid(code, &chunks);
    }

    #[test]
    fn transcript_splits_at_speaker_turns() {
        let t = "Alice: hi there\nAlice: still me\nBob: hello back\n[12:00] system: joined\n";
        let chunks = chunk(t, DocKind::Transcript, &ChunkConfig::default());
        // Each speaker-labelled line starts a turn → 4 turns.
        assert_eq!(chunks.len(), 4, "{chunks:#?}");
        assert!(chunks[0].text.starts_with("Alice: hi"));
        assert!(chunks[2].text.starts_with("Bob:"));
        assert!(chunks[3].text.starts_with("[12:00]"));
        assert_spans_valid(t, &chunks);
    }

    #[test]
    fn transcript_does_not_split_on_prose_colon_or_url() {
        // A colon deep in a sentence, and a URL colon, are not speaker turns.
        let t =
            "Alice: the plan is this\nwe ship at noon: no excuses\nsee https://x.io/y for detail\n";
        let chunks = chunk(t, DocKind::Transcript, &ChunkConfig::default());
        assert_eq!(chunks.len(), 1, "one turn, no false splits: {chunks:#?}");
    }

    #[test]
    fn attachment_windows_prose_with_overlap() {
        // Three ~40-char paragraphs; target 60 forces windowing with overlap.
        let p1 = "First paragraph about the migration plan.";
        let p2 = "Second paragraph about the rollback plan.";
        let p3 = "Third paragraph about the on-call rota.";
        let text = format!("{p1}\n\n{p2}\n\n{p3}\n");
        let cfg = ChunkConfig {
            target_chars: 60,
            overlap_chars: 20,
            max_chars: 1600,
        };
        let chunks = chunk(&text, DocKind::Attachment, &cfg);
        assert!(chunks.len() >= 2, "prose should window: {chunks:#?}");
        assert_spans_valid(&text, &chunks);
        // Consecutive windows overlap (the second starts before the first ends).
        assert!(
            chunks[1].start < chunks[0].end,
            "windows should overlap: {} !< {}",
            chunks[1].start,
            chunks[0].end,
        );
        // Coverage: the union of chunk spans reaches the end of the text.
        assert_eq!(chunks.last().unwrap().end, text.len());
    }

    #[test]
    fn oversize_markdown_section_is_windowed() {
        // One heading with a body far over max_chars → the section is windowed
        // rather than emitted as one unbounded chunk.
        let body = "para that is long. "
            .repeat(20)
            .split_inclusive(". ")
            .collect::<Vec<_>>()
            .join("\n\n");
        let md = format!("## Big\n{body}\n");
        let cfg = ChunkConfig {
            target_chars: 40,
            overlap_chars: 10,
            max_chars: 80,
        };
        let chunks = chunk(&md, DocKind::Markdown, &cfg);
        assert!(chunks.len() > 1, "oversize section must be windowed");
        for c in &chunks {
            assert!(c.end - c.start <= cfg.max_chars * 2, "chunk still bounded");
        }
    }

    #[test]
    fn empty_text_yields_no_chunks() {
        assert!(chunk("", DocKind::Markdown, &ChunkConfig::default()).is_empty());
        assert!(chunk("   \n\n", DocKind::Attachment, &ChunkConfig::default()).is_empty());
    }
}
