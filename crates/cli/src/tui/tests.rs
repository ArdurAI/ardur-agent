use super::{app::App, driver};
use crate::{Config, FusedEngine, StateDirs, Theme};
use ardur_provider_runtime::ChatMessage;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures::stream;
use ratatui::{Terminal, backend::TestBackend};

async fn engine() -> (tempfile::TempDir, FusedEngine) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let dirs = StateDirs {
        root: root.clone(),
        memory: root.join("memory"),
        journals: root.join("journals"),
        receipts: root.join("receipts"),
        keys: root.join("keys"),
    };
    dirs.create().unwrap();
    dirs.write_starter_cedar_policy_if_absent().unwrap();
    let engine = FusedEngine::new(&Config::default(), &dirs, 100)
        .await
        .unwrap();
    (temp, engine)
}

#[tokio::test]
async fn full_screen_consumer_runs_real_fused_turn_and_retains_receipted_history() {
    let (_temp, engine) = engine().await;
    let mut app = App::new(Theme::default());
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut history = vec![ChatMessage::user("local screen turn")];
    let mut events = stream::pending();
    driver::turn(&engine, &mut app, &mut terminal, &mut events, &mut history)
        .await
        .unwrap();
    assert_eq!(history.len(), 2);
    assert!(!history[1].content.is_empty());
    assert!(app.status.cost.is_some(), "must consume the receipt update");
    assert!(!app.status.pending);
    assert_eq!(app.status.verdict, crate::Verdict::InsufficientEvidence);
    assert_eq!(app.status.budget, Some(engine.remaining_cents()));
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("CHAT") && text.contains("INPUT") && text.contains("unverified"));
    assert!(app.transcript.iter().any(|b| b.text == history[1].content));
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_drops_source_drains_and_allows_next_turn() {
    let (_temp, engine) = engine().await;
    let mut app = App::new(Theme::default());
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
    let mut history = vec![ChatMessage::user("cancelled")];
    let cancelled = std::cell::Cell::new(false);
    let mut events = stream::poll_fn(|_| {
        if engine
            .settlement_supervisor()
            .status()
            .turns
            .iter()
            .any(|t| t.view.is_some())
            && !cancelled.replace(true)
        {
            std::task::Poll::Ready(Some(Ok(key(KeyCode::Char('c'), KeyModifiers::CONTROL))))
        } else {
            std::task::Poll::Pending
        }
    });
    driver::turn(&engine, &mut app, &mut terminal, &mut events, &mut history)
        .await
        .unwrap();
    assert!(
        cancelled.get(),
        "must cancel an admitted, owned turn, not an unpolled stream"
    );
    assert!(
        history.is_empty(),
        "no speculative text or unanswered user retained"
    );
    let journal = _temp
        .path()
        .join("journals/sessions")
        .join(engine.session_id().0.to_string())
        .join("journal.jsonl");
    let entries: Vec<ardur_session_journals::JournalEntry> = std::fs::read_to_string(journal)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        entries
            .iter()
            .filter(|e| matches!(
                e,
                ardur_session_journals::JournalEntry::CostFinalized { .. }
            ))
            .count(),
        1,
        "actual source drop must project cancellation"
    );
    assert!(app.status.error().unwrap().contains("Cancelled"));
    let state = engine.settlement_supervisor().status();
    assert!(state.turns.is_empty() && state.busy.is_none() && state.executing.is_none());
    history.push(ChatMessage::user("next turn"));
    driver::turn(
        &engine,
        &mut app,
        &mut terminal,
        &mut stream::pending(),
        &mut history,
    )
    .await
    .unwrap();
    assert_eq!(history.len(), 2);
    assert!(
        app.status.error().is_none(),
        "failure state resets per turn"
    );
    engine.shutdown().await.unwrap();
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

#[test]
fn keys_submit_multiline_recall_draft_and_refuse_overlapping_turns() {
    use super::app::Action;
    let mut app = App::new(Theme::default());
    app.event(Event::Paste("first界".into()));
    app.event(key(KeyCode::Enter, KeyModifiers::ALT));
    app.event(Event::Paste("second".into()));
    assert_eq!(
        app.event(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("first界\nsecond".into())
    );
    assert!(app.input.text.is_empty());
    app.status.begin();
    app.event(Event::Paste("draft".into()));
    assert_eq!(
        app.event(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    assert_eq!(app.input.text, "draft");
    app.status.pending = false;
    app.event(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input.text, "first界\nsecond");
    app.event(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.input.text, "draft");
    app.event(key(KeyCode::Home, KeyModifiers::NONE));
    app.event(key(KeyCode::Delete, KeyModifiers::NONE));
    app.event(key(KeyCode::Right, KeyModifiers::NONE));
    app.event(key(KeyCode::Backspace, KeyModifiers::NONE));
    assert_eq!(app.input.text, "aft");
}

#[test]
fn focus_scroll_palette_and_theme_work_without_enabling_color() {
    use super::app::Action;
    let mut app = App::new(Theme::default().plain());
    app.event(key(KeyCode::Tab, KeyModifiers::NONE));
    assert!(!app.input_focus);
    app.event(key(KeyCode::PageUp, KeyModifiers::NONE));
    assert!(app.scroll > 0);
    app.event(key(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(app.scroll, 0);
    app.event(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert!(!app.theme.is_styled());
    app.event(key(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert!(app.palette.is_some());
    app.event(key(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.palette.is_none());
    assert!(app.transcript.back().unwrap().text.contains("Alt-Enter"));
    assert_eq!(
        app.event(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
        Action::Exit
    );
}

#[test]
fn unicode_markdown_code_and_tables_align_in_cells_without_splitting_graphemes() {
    for theme in [Theme::default(), Theme::default().plain()] {
        let mut app = App::new(theme);
        app.reduce(crate::Update::ContentDelta(
            "```text\n界e\u{301}👩‍💻\n```\n\n| 名称 | 値 |\n| - | - |\n| 👩‍💻界 | e\u{301} |".into(),
        ));
        app.prepare(80);
        let lines: Vec<_> = app.lines.iter().map(ToString::to_string).collect();
        let mut expected_width = 0;
        let mut body_count = 0;
        for line in &lines {
            if line.starts_with('┌') {
                expected_width = unicode_width::UnicodeWidthStr::width(line.as_str());
            }
            if line.starts_with('│') {
                body_count += 1;
                assert!(
                    line.ends_with('│'),
                    "body must keep its right border: {line:?}"
                );
                assert_eq!(
                    unicode_width::UnicodeWidthStr::width(line.as_str()),
                    expected_width,
                    "cell alignment: {line:?}"
                );
            }
        }
        assert_eq!(body_count, 3);
        assert!(
            lines.iter().any(|l| l.contains("界e\u{301}👩‍💻")),
            "preserve full code graphemes"
        );
        assert!(
            lines.iter().any(|l| l.contains("👩‍💻界")),
            "preserve full table graphemes"
        );
    }
}

#[test]
fn palette_is_rendered_and_narrow_terminal_does_not_panic() {
    let mut app = App::new(Theme::default());
    app.event(key(KeyCode::Char('k'), KeyModifiers::CONTROL));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    driver::draw(&mut app, &mut terminal).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("COMMANDS"));
    assert!(
        text.contains("/help")
            && text.contains("/cost")
            && text.contains("/theme")
            && text.contains("/quit")
    );
    terminal.backend_mut().resize(10, 5);
    driver::draw(&mut app, &mut terminal).unwrap();
}

#[tokio::test]
async fn terminal_read_failure_after_commit_preserves_original_error_and_durable_history() {
    let (temp, engine) = engine().await;
    let journal = temp
        .path()
        .join("journals/sessions")
        .join(engine.session_id().0.to_string())
        .join("journal.jsonl");
    let mut app = App::new(Theme::default());
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut history = vec![ChatMessage::user("commit before terminal failure")];
    let mut events = stream::poll_fn(|_| {
        let committed = std::fs::read_to_string(&journal)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<ardur_session_journals::JournalEntry>(l).ok())
            .any(|e| {
                matches!(
                    e,
                    ardur_session_journals::JournalEntry::AssistantMessage { .. }
                )
            });
        if committed {
            std::task::Poll::Ready(Some(Err(std::io::Error::other(
                "original terminal failure",
            ))))
        } else {
            std::task::Poll::Pending
        }
    });
    let error = driver::turn(&engine, &mut app, &mut terminal, &mut events, &mut history)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "original terminal failure");
    assert_eq!(
        app.status.cost, None,
        "no Receipt notification delivered yet"
    );
    assert_eq!(
        history.len(),
        2,
        "journal commit must survive terminal failure"
    );
    assert!(!history[1].content.is_empty());
    let state = engine.settlement_supervisor().status();
    assert!(state.turns.is_empty() && state.busy.is_none() && state.executing.is_none());
    engine.shutdown().await.unwrap();
}

#[test]
fn no_color_frames_remain_unstyled_after_theme_switch_and_controls_are_neutralized() {
    let mut app = App::new(Theme::default().plain());
    app.event(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
    app.reduce(crate::Update::ContentDelta(
        "**styled** \x1b[31m \x1b]52;c;payload\x07 \u{202e}abc".into(),
    ));
    app.event(Event::Paste("界\x00\x1b[2J".into()));
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
    driver::draw(&mut app, &mut terminal).unwrap();
    for c in &terminal.backend().buffer().content {
        assert_eq!(c.fg, ratatui::style::Color::Reset);
        assert_eq!(c.bg, ratatui::style::Color::Reset);
        assert!(c.modifier.is_empty());
        assert!(
            !c.symbol()
                .chars()
                .any(|ch| ch.is_control() || ch == '\u{202e}')
        );
    }
}

#[tokio::test]
async fn unwind_drops_real_source_before_retained_owner_drains() {
    use futures::{FutureExt, StreamExt};
    let (temp, engine) = engine().await;
    let mut history = vec![ChatMessage::user("unwind after content")];
    let result = std::panic::AssertUnwindSafe(engine.consume_stream(&history, async |source| {
        let mut updates = crate::UpdateStream::new(source);
        while let Some(update) = updates.next().await {
            if let crate::Update::ContentDelta(text) = update {
                assert!(!text.is_empty());
                panic!("consumer unwind after admitted provider output");
            }
        }
        Ok(())
    }))
    .catch_unwind()
    .await;
    assert!(
        result.is_err(),
        "must actually poll through provider output"
    );
    engine.reconcile_history(&mut history).await.unwrap();
    assert!(history.is_empty());
    let journal = temp
        .path()
        .join("journals/sessions")
        .join(engine.session_id().0.to_string())
        .join("journal.jsonl");
    let entries: Vec<ardur_session_journals::JournalEntry> = std::fs::read_to_string(journal)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        entries
            .iter()
            .filter(|e| matches!(
                e,
                ardur_session_journals::JournalEntry::CostFinalized { .. }
            ))
            .count(),
        1
    );
    assert!(engine.settlement_supervisor().status().turns.is_empty());
    engine.shutdown().await.unwrap();
}

#[test]
fn adversarial_unicode_narrow_tables_and_tool_arguments_leave_no_internal_tokens() {
    use unicode_segmentation::UnicodeSegmentation;
    for width in [80, 120] {
        let mut app = App::new(Theme::default());
        let long = "👩‍💻界e\u{301}".repeat(80);
        app.reduce(crate::Update::ContentDelta(format!(
            "| A | B |\n| - | - |\n| {long} | {long} |\n\n```text\n{long}\n```"
        )));
        app.reduce(crate::Update::ToolCallResult {
            id: "call".into(),
            call: Some(crate::ToolCallInfo {
                name: "工具".into(),
                arguments: long,
            }),
            result: serde_json::json!({"not_displayed": "PRIVATE"}),
        });
        app.prepare(width);
        let mut count = 0;
        for line in &app.lines {
            let text = line.to_string();
            assert!(unicode_width::UnicodeWidthStr::width(text.as_str()) <= (width - 2) as usize);
            assert!(!text.contains("PRIVATE"));
            assert!(
                !text
                    .chars()
                    .any(|c| ('\u{f0000}'..='\u{ffffd}').contains(&c)),
                "internal layout token escaped"
            );
            for g in text.graphemes(true) {
                if g.contains('👩') || g.contains('💻') {
                    assert_eq!(g, "👩‍💻");
                    count += 1;
                }
            }
        }
        assert!(count > 0, "must actually render adversarial output");
    }
}

#[test]
fn entity_expanded_controls_cannot_inject_styles_into_plain_frames() {
    let mut app = App::new(Theme::default().plain());
    app.reduce(crate::Update::ContentDelta(
        "&#x1b;[31mred&#27;[0m &Tab;safe &#x202e;abc".into(),
    ));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    driver::draw(&mut app, &mut terminal).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        text.contains("red") && text.contains("safe"),
        "fixture must render"
    );
    for cell in &terminal.backend().buffer().content {
        assert_eq!(
            cell.fg,
            ratatui::style::Color::Reset,
            "entity must not become trusted SGR"
        );
        assert_eq!(cell.bg, ratatui::style::Color::Reset);
        assert!(cell.modifier.is_empty());
        assert!(
            !cell
                .symbol()
                .chars()
                .any(|c| c.is_control() || c == '\u{202e}')
        );
    }
}

#[test]
fn decoded_markdown_text_keeps_cell_width_and_literal_semantics() {
    let mut app = App::new(Theme::default());
    app.reduce(crate::Update::ContentDelta(
        "| A | B |\n| - | - |\n| &#x754c; | x |\n| e&#x301; | x |\n\n&#42;literal&#42; &lt;tag&gt;"
            .into(),
    ));
    app.prepare(80);
    let rows: Vec<_> = app.lines.iter().map(ToString::to_string).collect();
    assert!(
        rows.iter().any(|l| l.contains("*literal* <tag>")),
        "decoded punctuation must not be re-parsed as markup"
    );
    let mut width = 0;
    for line in rows {
        if line.starts_with('┌') {
            width = unicode_width::UnicodeWidthStr::width(line.as_str());
        }
        if line.starts_with('│') {
            assert_eq!(unicode_width::UnicodeWidthStr::width(line.as_str()), width);
        }
    }
    assert!(width > 0);
}

#[test]
fn themed_canvas_preserves_contrast_in_input_and_palette() {
    use ratatui::style::Color;
    for (theme, foreground, background) in [
        (
            Theme::named(crate::ThemeName::Dawn),
            Color::Indexed(235),
            Color::Indexed(230),
        ),
        (
            Theme::named(crate::ThemeName::Night),
            Color::Indexed(254),
            Color::Indexed(234),
        ),
        (
            Theme::named(crate::ThemeName::Terminal),
            Color::Reset,
            Color::Reset,
        ),
        (
            Theme::named(crate::ThemeName::Dawn).plain(),
            Color::Reset,
            Color::Reset,
        ),
    ] {
        let mut app = App::new(theme);
        app.input.insert("q");
        app.palette = Some(0);
        app.prepare(80);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| super::view::render(&app, frame))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            buffer.content.iter().all(|cell| cell.bg == background),
            "canvas and cleared palette must retain the selected theme background"
        );
        let input = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "q")
            .unwrap();
        assert_eq!(input.fg, foreground, "unstyled input must remain readable");
    }
}

#[test]
fn transcript_and_input_growth_are_bounded() {
    use super::app::{Kind, MAX_BLOCK_BYTES, MAX_BLOCKS, MAX_TRANSCRIPT_BYTES};
    let mut app = App::new(Theme::default());
    for _ in 0..(MAX_BLOCKS * 2) {
        app.push(Kind::User, &"界".repeat(MAX_BLOCK_BYTES));
    }
    assert!(app.transcript.len() <= MAX_BLOCKS);
    assert!(app.transcript.iter().map(|b| b.text.len()).sum::<usize>() <= MAX_TRANSCRIPT_BYTES);
    assert!(
        app.transcript
            .iter()
            .all(|b| b.text.len() <= MAX_BLOCK_BYTES)
    );
    app.prepare(80);
    assert!(app.lines.len() <= 8192);
    app.event(Event::Paste("界".repeat(super::input::MAX_INPUT_BYTES)));
    assert!(app.input.text.len() <= super::input::MAX_INPUT_BYTES);
}
