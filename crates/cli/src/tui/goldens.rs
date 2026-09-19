//! Styled TestBackend frames. Regeneration is explicit and forbidden in CI.
use super::{
    app::{App, Kind},
    driver,
};
use crate::{Theme, Update};
use ardur_fused_runtime::StageKind;
use ardur_provider_runtime::Usage;
use ardur_runtime::RuntimeError;
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize)]
struct StyleRun {
    start: usize,
    len: usize,
    fg: String,
    bg: String,
    modifiers: String,
}
#[derive(Serialize)]
struct Snapshot {
    width: u16,
    height: u16,
    // Each row includes continuation cells for wide glyphs, and trailing spaces.
    rows: Vec<Vec<String>>,
    styles: Vec<StyleRun>,
}
fn snapshot(buffer: &Buffer) -> String {
    let mut styles: Vec<StyleRun> = vec![];
    for (i, c) in buffer.content.iter().enumerate() {
        let (fg, bg, modifiers) = (
            format!("{:?}", c.fg),
            format!("{:?}", c.bg),
            format!("{:?}", c.modifier),
        );
        if let Some(last) = styles.last_mut() {
            if last.fg == fg && last.bg == bg && last.modifiers == modifiers {
                last.len += 1;
                continue;
            }
        }
        styles.push(StyleRun {
            start: i,
            len: 1,
            fg,
            bg,
            modifiers,
        });
    }
    let rows = buffer
        .content
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(|c| c.symbol().to_owned()).collect())
        .collect();
    serde_json::to_string_pretty(&Snapshot {
        width: buffer.area.width,
        height: buffer.area.height,
        rows,
        styles,
    })
    .unwrap()
        + "\n"
}
fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tui")
}

#[test]
fn styled_goldens_streaming_denial_and_approval_at_80_and_120() {
    let regenerate = std::env::var("ARDUR_UPDATE_TUI_GOLDENS").as_deref() == Ok("1");
    assert!(
        !regenerate || std::env::var_os("CI").is_none(),
        "never regenerate goldens in CI"
    );
    let mut examined = vec![];
    for scenario in ["streaming", "denial", "approval"] {
        for width in [80, 120] {
            let mut app = App::new(Theme::default());
            app.title = "ardur · local fixture · full pipeline".into();
            app.status.budget = Some(800);
            app.push(Kind::User, "Show the current result.");
            app.status.begin();
            match scenario {
                "streaming" => {
                    app.reduce(Update::StageStart {
                        stage: StageKind::ProviderStream,
                    });
                    app.reduce(Update::ContentDelta("## Streaming result\n\n**Ready** to render `cells`.\n\n```text\n界e\u{301}👩‍💻\n```".into()));
                    let usage = Usage {
                        tokens_in: 2048,
                        tokens_out: 12,
                        cost_cents: None,
                    };
                    app.reduce(Update::Usage {
                        usage,
                        total: usage,
                    });
                    app.elapsed = std::time::Duration::from_millis(1250);
                    app.tick = 1;
                }
                "denial" => {
                    app.reduce(Update::Error(RuntimeError::PolicyDenied {
                        reason: "PRIVATE\x1b]52;secret\x07".into(),
                    }));
                    app.status.pending = false;
                }
                "approval" => {
                    app.reduce(Update::Error(RuntimeError::ApprovalRequired {
                        approval_id: "PRIVATE".into(),
                        tool: "PRIVATE".into(),
                        reason: "PRIVATE".into(),
                    }));
                    app.status.pending = false;
                }
                _ => unreachable!(),
            }
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            driver::draw(&mut app, &mut terminal).unwrap();
            let buffer = terminal.backend().buffer();
            let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
            assert!(text.contains("? unverified"));
            assert!(text.contains("turn receipt unknown"));
            assert!(!text.contains("PRIVATE") && !text.contains('\x1b'));
            assert!(
                buffer
                    .content
                    .iter()
                    .any(|c| c.fg != ratatui::style::Color::Reset)
            );
            let name = format!("{scenario}-{width}.json");
            let rendered = snapshot(buffer);
            let path = fixture().join(&name);
            if regenerate {
                std::fs::write(&path, &rendered).unwrap();
            }
            assert_eq!(
                rendered,
                std::fs::read_to_string(&path)
                    .expect("missing styled golden; explicitly regenerate outside CI"),
                "{name}"
            );
            examined.push(name);
        }
    }
    examined.sort();
    assert_eq!(examined.len(), 6, "six rendered frames, not six tests");
    let mut inventory: Vec<_> = std::fs::read_dir(fixture())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".json"))
        .collect();
    inventory.sort();
    assert_eq!(inventory, examined, "exact snapshot inventory");
}
