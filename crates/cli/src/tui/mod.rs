//! Opt-in full-screen terminal surface.
mod app;
mod cell_text;
mod driver;
#[cfg(test)]
mod goldens;
mod input;
mod keys;
mod status;
mod terminal;
#[cfg(test)]
mod tests;
mod text;
mod view;
use crate::{ChatArgs, CliError};
use std::io::IsTerminal;

pub(crate) fn validate_env(args: &ChatArgs) -> Result<bool, CliError> {
    let enabled = match std::env::var("ARDUR_TUI").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("0") => false,
        Ok("1") => true,
        _ => return Err(CliError::State("ARDUR_TUI must be 0 or 1".into())),
    };
    if !enabled {
        return Ok(false);
    }
    for (conflict, flag) in [
        (args.echo, "--echo"),
        (args.plain, "--plain"),
        (args.no_stream, "--no-stream"),
    ] {
        if conflict {
            return Err(CliError::State(format!(
                "ARDUR_TUI=1 is incompatible with {flag}"
            )));
        }
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(CliError::State(
            "ARDUR_TUI=1 requires interactive stdin and stdout; use the default REPL for pipes"
                .into(),
        ));
    }
    Ok(true)
}

/// TUI-only startup: validate all presentation settings before opening state.
pub(crate) fn run(args: ChatArgs) -> Result<(), CliError> {
    let capacity = match std::env::var("ARDUR_TUI_CONTEXT_TOKENS") {
        Err(std::env::VarError::NotPresent) => None,
        Ok(raw) => Some(raw.parse::<u64>().ok().filter(|v| *v > 0).ok_or_else(|| {
            CliError::State("ARDUR_TUI_CONTEXT_TOKENS must be a positive integer".into())
        })?),
        Err(_) => {
            return Err(CliError::State(
                "ARDUR_TUI_CONTEXT_TOKENS must be a positive integer".into(),
            ));
        }
    };
    let animate = match std::env::var("ARDUR_TUI_ANIMATE").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("1") => true,
        Ok("0") => false,
        _ => return Err(CliError::State("ARDUR_TUI_ANIMATE must be 0 or 1".into())),
    };
    let mut config = crate::Config::load(args.config.clone()).map_err(public_error)?;
    config.budget_cents = crate::resolve_budget_cents(args.budget_cents, config.budget_cents);
    let session_id = args
        .session_id
        .as_deref()
        .map(crate::parse_session_id)
        .transpose()
        .map_err(public_error)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(CliError::Io)?;
    runtime.block_on(async {
        use app::{App, Kind};
        use futures::FutureExt;
        let dirs = crate::StateDirs::resolve().map_err(public_error)?;
        dirs.create().map_err(public_error)?;
        if let Some(id) = session_id {
            if !crate::session_journal_path(&dirs, id).is_file() {
                return Err(CliError::State("session not found".into()));
            }
        }
        let engine =
            crate::FusedEngine::new_for_session(&config, &dirs, config.budget_cents, session_id)
                .await
                .map_err(public_error)?;
        // The owner survives every terminal/turn future and any caught unwind.
        let settlements = engine.settlements.clone();
        let result = std::panic::AssertUnwindSafe(async {
            let mut history = vec![];
            engine.reconcile_history(&mut history).await?;
            let mut app = App::new(crate::Theme::from_env());
            app.animate = animate && !app.no_color;
            app.status.capacity = capacity;
            app.status.budget = Some(engine.remaining_cents());
            app.title = text::sanitize(
                &format!(
                    "ardur{} · session {} · {}",
                    if engine.offline() {
                        " · offline stub"
                    } else {
                        ""
                    },
                    engine.session_id().0,
                    config.model
                ),
                256,
            );
            for message in &history {
                let kind = if message.role == ardur_provider_runtime::Role::User {
                    Kind::User
                } else {
                    Kind::Response
                };
                app.push(kind, &message.content);
            }
            app.push(
                Kind::Notice,
                "Full pipeline · /help for keys · approval prompts only",
            );
            let mut guard = terminal::Guard::acquire()?;
            let mut screen =
                ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
            let result = driver::run_loop(
                &engine,
                &mut app,
                &mut screen,
                &mut crossterm::event::EventStream::new(),
                &mut history,
            )
            .await;
            // Drop the renderer before restoring the screen/cursor it controls.
            drop(screen);
            let restored = guard.restore();
            result.and(restored).map_err(CliError::Io)
        })
        .catch_unwind()
        .await;
        let closed = settlements.finish().await;
        match result {
            Ok(result) => result.and(closed).map_err(public_error),
            Err(_) => Err(CliError::State(
                "TUI stopped after an internal failure; terminal restoration attempted".into(),
            )),
        }
    })
}

fn public_error(error: CliError) -> CliError {
    let label = match error {
        CliError::Runtime(ref error) => status::safe_error(error),
        CliError::Config(_) => "TUI configuration could not be loaded",
        CliError::Io(_) => "TUI terminal I/O failed; restoration attempted",
        CliError::Provider(_) => "TUI provider unavailable",
        CliError::CapToken(_) => "TUI capability setup failed",
        CliError::State(_) => {
            "TUI persistent state unavailable or unresolved; inspect state before retrying"
        }
    };
    CliError::State(label.into())
}
