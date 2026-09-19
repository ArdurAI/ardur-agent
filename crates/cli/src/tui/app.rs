pub(super) use super::view::render;
use super::{cell_text::CellText, text::sanitize};
use super::{input::Input, status::Status};
use crate::{Role, Theme, Update};
use ratatui::text::Line;
use std::collections::VecDeque;
use std::time::Duration;

pub(super) const MAX_BLOCK_BYTES: usize = 32 * 1024;
pub(super) const MAX_TRANSCRIPT_BYTES: usize = 256 * 1024;
pub(super) const MAX_BLOCKS: usize = 128;

#[derive(PartialEq)]
pub(super) enum Kind {
    Response,
    User,
    Notice,
    Tool(String),
}
pub(super) struct Block {
    pub kind: Kind,
    pub text: String,
    cache: Vec<Line<'static>>,
    dirty: bool,
}
#[derive(Debug, PartialEq)]
pub(super) enum Action {
    None,
    Submit(String),
    Cancel,
    Exit,
}

pub(super) struct App {
    pub theme: Theme,
    pub transcript: VecDeque<Block>,
    pub lines: Vec<Line<'static>>,
    pub input: Input,
    pub status: Status,
    pub title: String,
    pub elapsed: Duration,
    pub tick: usize,
    pub animate: bool,
    pub scroll: usize,
    pub input_focus: bool,
    pub palette: Option<usize>,
    pub history: VecDeque<String>,
    pub history_index: Option<usize>,
    pub draft: Input,
    pub no_color: bool,
    width: u16,
    cached_theme: Theme,
    dirty: bool,
}
impl App {
    pub fn new(theme: Theme) -> Self {
        Self {
            no_color: !theme.is_styled(),
            palette: None,
            history: VecDeque::new(),
            history_index: None,
            draft: Input::default(),
            cached_theme: theme.clone(),
            theme,
            transcript: VecDeque::new(),
            lines: vec![],
            width: 0,
            dirty: true,
            input: Input::default(),
            status: Status::default(),
            title: "ardur".into(),
            elapsed: Duration::ZERO,
            tick: 0,
            animate: true,
            scroll: 0,
            input_focus: true,
        }
    }
    pub fn push(&mut self, kind: Kind, text: &str) {
        self.transcript.push_back(Block {
            kind,
            text: sanitize(text, MAX_BLOCK_BYTES),
            cache: vec![],
            dirty: true,
        });
        self.trim();
    }
    fn trim(&mut self) {
        while self.transcript.len() > MAX_BLOCKS
            || self.transcript.iter().map(|b| b.text.len()).sum::<usize>() > MAX_TRANSCRIPT_BYTES
        {
            self.transcript.pop_front();
        }
        self.dirty = true;
    }
    pub fn reduce(&mut self, update: Update) {
        self.status.reduce(&update);
        if let Update::Error(ref error) = update {
            self.push(Kind::Notice, super::status::safe_error(error));
        }
        if let Update::ToolCallResult {
            call: Some(ref call),
            ..
        } = update
        {
            self.push(Kind::Tool(sanitize(&call.name, 128)), &call.arguments);
        }
        if let Update::ContentDelta(delta) = update {
            if self
                .transcript
                .back()
                .is_none_or(|b| b.kind != Kind::Response)
            {
                self.push(Kind::Response, "");
            }
            let block = self.transcript.back_mut().unwrap();
            block.text.push_str(&sanitize(
                &delta,
                MAX_BLOCK_BYTES.saturating_sub(block.text.len()),
            ));
            block.dirty = true;
            self.trim();
        }
    }
    pub fn prepare(&mut self, width: u16) {
        let all = self.width != width || self.theme != self.cached_theme;
        if !all && !self.dirty {
            return;
        }
        self.width = width;
        self.cached_theme = self.theme.clone();
        self.lines.clear();
        for block in &mut self.transcript {
            if all || block.dirty {
                let w = width.saturating_sub(2) as usize;
                let mut cells = CellText::default();
                let generated = match &block.kind {
                    Kind::Response => crate::markdown::render_markdown_for_cells(
                        &block.text,
                        &self.theme,
                        w,
                        |text| cells.encode(&sanitize(text, MAX_BLOCK_BYTES)),
                    ),
                    Kind::User => self
                        .theme
                        .paint(Role::Primary, &format!("› {}", cells.encode(&block.text))),
                    Kind::Notice => self.theme.paint(Role::Warn, &cells.encode(&block.text)),
                    Kind::Tool(name) => {
                        let encoded = cells.encode(&block.text);
                        crate::render_tool_call_box(&cells.encode(name), &encoded, &self.theme, w)
                    }
                };
                block.cache = super::text::wrap(cells.decode(&generated), w);
                if block.cache.len() > 512 {
                    block.cache.truncate(512);
                    block.cache.push(Line::from("… display truncated"));
                }
                block.dirty = false;
            }
            self.lines.extend(block.cache.iter().cloned());
            self.lines.push(Line::default());
        }
        if self.lines.len() > 8192 {
            self.lines.drain(..self.lines.len() - 8192);
        }
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::{Color, Modifier},
    };
    #[test]
    fn typed_denial_is_public_and_unverified_in_the_frame() {
        let mut app = App::new(Theme::default());
        app.reduce(Update::Error(ardur_runtime::RuntimeError::PolicyDenied {
            reason: "private diagnostic".into(),
        }));
        app.prepare(80);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(&app, f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Denied: policy"));
        assert!(text.contains("? unverified"));
        assert!(!text.contains("private diagnostic") && !text.contains("compliant"));
    }
    #[test]
    fn shared_markdown_streams_into_safe_styled_cells() {
        let mut app = App::new(Theme::default());
        app.reduce(Update::ContentDelta("## Heading\n\n**bold** and `code`\n\n|A|B|\n|-|-|\n|1|2|\n\n```rust\nlet n = 3;\n```\n\x1b]52;c;payload\x07".into()));
        app.prepare(80);
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| render(&app, f)).unwrap();
        let b = terminal.backend().buffer();
        let text: String = b.content.iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Heading"), "must render a streamed heading");
        assert!(text.contains("let n = 3;"));
        assert!(!text.contains('\x1b') && !text.contains('\x07'));
        assert!(
            b.content
                .iter()
                .any(|c| c.fg != Color::Reset && c.modifier.contains(Modifier::BOLD))
        );
        assert!(
            app.lines.iter().any(|l| l.to_string().contains("┌")),
            "shared code/table framing"
        );
        let old = app.lines.clone();
        app.prepare(80);
        assert_eq!(app.lines, old, "unchanged layout is stable");
    }
}
