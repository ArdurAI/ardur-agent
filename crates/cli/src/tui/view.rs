use super::{app::App, text::role};
use crate::{Role, ThemeName};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::Color,
    widgets::{Block, Borders, Paragraph},
};

/// Pure rendering: clocks, reduction and cached Markdown layout are supplied.
pub(super) fn render(app: &App, frame: &mut Frame<'_>) {
    let area = frame.area();
    // Full-screen themes own the canvas (design §B.2), unlike the unchanged
    // REPL. A dawn foreground on the terminal's dark default is unreadable.
    let background = if app.theme.is_styled() {
        match app.theme.name() {
            ThemeName::Dawn => Color::Indexed(230),
            ThemeName::Night => Color::Indexed(234),
            ThemeName::Terminal => Color::Reset,
        }
    } else {
        Color::Reset
    };
    let canvas = role(&app.theme, Role::Fg).bg(background);
    frame.render_widget(Block::default().style(canvas), area);
    if area.width < 72 || area.height < 16 {
        frame.render_widget(
            Paragraph::new("TUI needs 72 columns and 16 rows; resize or use the REPL."),
            area,
        );
        return;
    }
    let (input_rows, (cx, cy)) = app.input.rows(area.width.saturating_sub(2) as usize);
    let input_height = input_rows.len().clamp(1, 5) as u16;
    let regions = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(input_height + 2),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(app.title.as_str()).style(role(&app.theme, Role::Primary)),
        regions[0],
    );
    let transcript_height = regions[1].height.saturating_sub(2) as usize;
    let max_scroll = app.lines.len().saturating_sub(transcript_height);
    let top = max_scroll.saturating_sub(app.scroll.min(max_scroll));
    frame.render_widget(
        Paragraph::new(
            app.lines
                .iter()
                .skip(top)
                .take(transcript_height)
                .cloned()
                .collect::<Vec<_>>(),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(if app.input_focus {
                    " CHAT "
                } else {
                    " CHAT · focused "
                })
                .border_style(role(
                    &app.theme,
                    if app.input_focus {
                        Role::Dim
                    } else {
                        Role::Primary
                    },
                )),
        ),
        regions[1],
    );
    frame.render_widget(
        Paragraph::new(app.status.activity(app.elapsed, app.tick, app.animate)).style(role(
            &app.theme,
            if app.status.error().is_some() {
                Role::Warn
            } else {
                Role::Dim
            },
        )),
        regions[2],
    );
    let statuses = Layout::vertical([Constraint::Length(1); 3]).split(regions[3]);
    frame.render_widget(
        Paragraph::new(app.status.budget_label()).style(role(&app.theme, Role::Dim)),
        statuses[0],
    );
    let (context, color) = app.status.context();
    frame.render_widget(
        Paragraph::new(context).style(role(&app.theme, color)),
        statuses[1],
    );
    let (verdict, color) = app.status.verdict_label();
    frame.render_widget(
        Paragraph::new(verdict).style(role(&app.theme, color)),
        statuses[2],
    );
    let offset = cy.saturating_sub(input_height as usize - 1);
    let input = input_rows
        .iter()
        .skip(offset)
        .take(input_height as usize)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    frame.render_widget(
        Paragraph::new(input).block(
            Block::default()
                .borders(Borders::ALL)
                .title(if app.status.pending {
                    " INPUT · turn running "
                } else {
                    " INPUT · Enter send / Alt-Enter newline "
                })
                .border_style(role(
                    &app.theme,
                    if app.input_focus {
                        Role::Primary
                    } else {
                        Role::Dim
                    },
                )),
        ),
        regions[4],
    );
    if app.input_focus && app.palette.is_none() {
        frame.set_cursor_position((
            regions[4].x + 1 + cx as u16,
            regions[4].y + 1 + (cy - offset) as u16,
        ));
    }
    frame.render_widget(
        Paragraph::new("^K palette · Tab focus · PgUp/PgDn · ^T theme · ^C cancel · ^D exit")
            .style(role(&app.theme, Role::Dim)),
        regions[5],
    );
    if let Some(selected) = app.palette {
        use ratatui::{
            layout::Rect,
            widgets::{Clear, List, ListItem, ListState},
        };
        let popup = Rect::new(
            area.x + (area.width - 48) / 2,
            area.y + (area.height - 8) / 2,
            48,
            8,
        );
        frame.render_widget(Clear, popup);
        let items: Vec<_> = super::keys::COMMANDS
            .iter()
            .map(|c| ListItem::new(*c))
            .collect();
        let list = List::new(items)
            .style(canvas)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" COMMANDS · Esc close "),
            )
            .highlight_symbol("› ")
            .highlight_style(role(&app.theme, Role::Accent));
        frame.render_stateful_widget(
            list,
            popup,
            &mut ListState::default().with_selected(Some(selected)),
        );
    }
}
