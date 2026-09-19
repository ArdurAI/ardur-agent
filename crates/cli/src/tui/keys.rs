use super::app::{Action, App, Kind};
use crate::{Theme, ThemeName};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

pub(super) const COMMANDS: [&str; 4] = ["/help", "/cost", "/theme", "/quit"];
const HELP: &str = "Enter send · Alt-Enter newline · Up/Down prompt history
Left/Right/Home/End edit · Backspace/Delete · bracketed paste
Tab focus · PgUp/PgDn scroll · End (chat) follow latest
Ctrl-K palette · Ctrl-T theme · Ctrl-C cancel turn / clear draft
Ctrl-D or /quit exit · /cost last receipt and ledger
/theme [night|dawn|terminal] · /help
Approval prompts only: use ardur approvals outside the TUI.";

impl App {
    pub fn event(&mut self, event: Event) -> Action {
        let Event::Key(key) = event else {
            if let Event::Paste(text) = event {
                if self.input_focus && self.palette.is_none() {
                    self.input.insert(&text);
                }
            }
            return Action::None;
        };
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl {
            match key.code {
                KeyCode::Char('d') => return Action::Exit,
                KeyCode::Char('c') => {
                    self.palette = None;
                    if self.status.pending {
                        return Action::Cancel;
                    }
                    self.input = Default::default();
                    self.history_index = None;
                    return Action::None;
                }
                KeyCode::Char('k') => {
                    self.palette = if self.palette.is_some() {
                        None
                    } else {
                        Some(0)
                    };
                    return Action::None;
                }
                KeyCode::Char('t') => {
                    self.cycle_theme();
                    return Action::None;
                }
                _ => {}
            }
        }
        if let Some(selected) = self.palette {
            match key.code {
                KeyCode::Esc => self.palette = None,
                KeyCode::Up => self.palette = Some(selected.saturating_sub(1)),
                KeyCode::Down => self.palette = Some((selected + 1).min(COMMANDS.len() - 1)),
                KeyCode::Enter => {
                    self.palette = None;
                    return self.command(COMMANDS[selected]);
                }
                _ => {}
            }
            return Action::None;
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => self.input_focus = !self.input_focus,
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10).min(8192),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::End if !self.input_focus => self.scroll = 0,
            KeyCode::Home if !self.input_focus => self.scroll = 8192,
            KeyCode::Up if !self.input_focus => {
                self.scroll = self.scroll.saturating_add(1).min(8192)
            }
            KeyCode::Down if !self.input_focus => self.scroll = self.scroll.saturating_sub(1),
            _ if !self.input_focus => {}
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                self.input.insert(
                    "
",
                )
            }
            KeyCode::Enter if !self.status.pending => {
                let prompt = self.input.text.trim().to_owned();
                if prompt.is_empty() {
                    return Action::None;
                }
                self.input = Default::default();
                self.history_index = None;
                if self.history.back() != Some(&prompt) {
                    self.history.push_back(prompt.clone());
                }
                if self.history.len() > 128 {
                    self.history.pop_front();
                }
                if prompt.starts_with('/') {
                    return self.command(&prompt);
                }
                self.scroll = 0;
                self.push(Kind::User, &prompt);
                return Action::Submit(prompt);
            }
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.cursor = 0,
            KeyCode::End => self.input.cursor = self.input.text.len(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Up => self.recall(true),
            KeyCode::Down => self.recall(false),
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.input.insert(&c.to_string())
            }
            _ => {}
        }
        Action::None
    }

    fn recall(&mut self, previous: bool) {
        if self.history.is_empty() {
            return;
        }
        let next = if previous {
            match self.history_index {
                None => {
                    self.draft = self.input.clone();
                    Some(self.history.len() - 1)
                }
                Some(i) => Some(i.saturating_sub(1)),
            }
        } else {
            self.history_index
                .and_then(|i| (i + 1 < self.history.len()).then_some(i + 1))
        };
        if let Some(i) = next {
            self.input.text = self.history[i].clone();
            self.input.cursor = self.input.text.len();
        } else if self.history_index.is_some() {
            self.input = self.draft.clone();
        }
        self.history_index = next;
    }

    fn cycle_theme(&mut self) {
        let name = match self.theme.name() {
            ThemeName::Night => ThemeName::Dawn,
            ThemeName::Dawn => ThemeName::Terminal,
            ThemeName::Terminal => ThemeName::Night,
        };
        self.set_theme(name);
    }
    fn set_theme(&mut self, name: ThemeName) {
        let theme = Theme::named(name);
        self.theme = if self.no_color { theme.plain() } else { theme };
    }
    fn command(&mut self, command: &str) -> Action {
        let mut args = command.split_whitespace();
        match args.next().unwrap_or("") {
            "/quit" | "/exit" => return Action::Exit,
            "/help" => self.push(Kind::Notice, HELP),
            "/cost" => self.push(Kind::Notice, &self.status.budget_label()),
            "/theme" => match args.next() {
                None => self.cycle_theme(),
                Some(name) => match ThemeName::parse(name) {
                    Some(name) => self.set_theme(name),
                    None => self.push(Kind::Notice, "Theme: night, dawn or terminal"),
                },
            },
            _ => self.push(
                Kind::Notice,
                "Unknown TUI command · /help lists supported commands",
            ),
        }
        Action::None
    }
}
