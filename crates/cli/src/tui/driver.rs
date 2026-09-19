//! Pull-based full-pipeline consumer. Terminal events never own the runtime.
use super::app::{Action, App, render};
use crate::{FusedEngine, Update, UpdateStream};
use ardur_provider_runtime::ChatMessage;
use ardur_runtime::RuntimeError;
use crossterm::event::Event;
use futures::{Stream, StreamExt};
use ratatui::{Terminal, backend::Backend};
use std::{io, time::Instant};

pub(super) fn draw<B: Backend>(app: &mut App, terminal: &mut Terminal<B>) -> io::Result<()> {
    app.prepare(terminal.size()?.width);
    terminal.draw(|frame| render(app, frame))?;
    Ok(())
}

pub(super) async fn turn<B, E>(
    engine: &FusedEngine,
    app: &mut App,
    terminal: &mut Terminal<B>,
    events: &mut E,
    history: &mut Vec<ChatMessage>,
) -> io::Result<bool>
where
    B: Backend,
    E: Stream<Item = io::Result<Event>> + Unpin,
{
    app.status.begin();
    app.elapsed = std::time::Duration::ZERO;
    app.tick = 0;
    let started = Instant::now();
    let result = engine.consume_stream(history, async |source| {
        let mut updates = UpdateStream::new(source);
        let mut ticker = tokio::time::interval(crate::TYPING_DOTS_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        draw(app, terminal)?;
        loop {
            tokio::select! {
                biased;
                event = events.next() => {
                    let exit = match event.transpose()? {
                        None => true,
                        Some(event) => match app.event(event) {
                            Action::Exit => true,
                            Action::Cancel => false,
                            Action::None | Action::Submit(_) => { draw(app, terminal)?; continue; }
                        },
                    };
                    app.reduce(Update::Error(RuntimeError::TurnCancelled));
                    break Ok(exit);
                }
                _ = ticker.tick() => {
                    app.elapsed = started.elapsed();
                    app.tick = app.tick.wrapping_add(1);
                    draw(app, terminal)?;
                }
                update = updates.next() => {
                    let Some(update) = update else { break Ok(false) };
                    app.reduce(update);
                    draw(app, terminal)?;
                }
            }
        }
    }).await;
    // This read is after consume_stream destroyed its owning source and drained
    // settlement. A commit can be durable even if Receipt was never delivered.
    let reconciled = engine
        .reconcile_history(history)
        .await
        .map_err(io::Error::other);
    app.status.pending = false;
    app.status.budget = Some(engine.remaining_cents());
    let result = result.and_then(|exit| reconciled.map(|()| exit));
    if result.is_ok() {
        draw(app, terminal)?;
    }
    result
}

pub(super) async fn run_loop<B, E>(
    engine: &FusedEngine,
    app: &mut App,
    terminal: &mut Terminal<B>,
    events: &mut E,
    history: &mut Vec<ChatMessage>,
) -> io::Result<()>
where
    B: Backend,
    E: Stream<Item = io::Result<Event>> + Unpin,
{
    draw(app, terminal)?;
    while let Some(event) = events.next().await {
        match app.event(event?) {
            Action::Exit => break,
            Action::Submit(prompt) => {
                history.push(ChatMessage::user(prompt));
                if turn(engine, app, terminal, events, history).await? {
                    break;
                }
            }
            Action::None | Action::Cancel => {}
        }
        draw(app, terminal)?;
    }
    Ok(())
}
