//! Interactive dashboard: a pure Elm-style core (model/update) with IO at the
//! edges (view/exec) and the event loop in `run`.

pub mod exec;
pub mod model;
pub mod update;
pub mod view;

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app;
use crate::cli::default_retries;
use crate::paths;
use model::{Model, ScheduleDefaults};

/// Map a crossterm event to a `Msg`, ignoring what the dashboard does not act on.
/// Pure so the key contract is testable without a terminal.
pub fn map_event(ev: Event) -> Option<update::Msg> {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => Some(update::Msg::Key(k.code)),
        _ => None,
    }
}

/// Restores the terminal on every exit path — normal return, `?`, or panic.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<TerminalGuard> {
        enable_raw_mode()?;
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn schedule_defaults() -> ScheduleDefaults {
    ScheduleDefaults {
        send_delay_secs: 0.75,
        settle_secs: std::env::var("NUDGE_SETTLE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5.0),
        retries: default_retries(),
    }
}

/// Open the dashboard. Resolves paths and ensures a daemon *before* raw mode so
/// a spawn or a stale-build handshake prints as normal output, not into the
/// alternate screen.
pub fn run() -> anyhow::Result<()> {
    let paths = paths::resolve();
    app::ensure_daemon(&paths)?;
    let socket = paths.socket.clone();

    // Restore the terminal even if a later panic unwinds through the loop.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        prev_hook(info);
    }));

    let _guard = TerminalGuard::enter().context("entering the alternate screen")?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal: Terminal<CrosstermBackend<Stdout>> = Terminal::new(backend)?;

    let mut model = Model::new(schedule_defaults(), jiff::Timestamp::now());
    // Prime the first frame with jobs (and, later, panes on first Tab).
    let tick = update::update(&mut model, update::Msg::Tick(jiff::Timestamp::now()));
    dispatch_effects(&mut model, tick, &socket);
    dispatch_effects(&mut model, vec![update::Effect::PollJobs], &socket);

    while !model.should_quit {
        terminal.draw(|f| view::view(&model, f))?;

        // ~4 wakeups/sec: responsive input, smooth 1s countdowns.
        let msg = if event::poll(Duration::from_millis(250))? {
            map_event(event::read()?)
        } else {
            Some(update::Msg::Tick(jiff::Timestamp::now()))
        };
        if let Some(msg) = msg {
            let effects = update::update(&mut model, msg);
            dispatch_effects(&mut model, effects, &socket);
        }
    }
    Ok(())
}

/// Run each effect (blocking) and feed its `Msg` straight back into `update`.
fn dispatch_effects(model: &mut Model, effects: Vec<update::Effect>, socket: &std::path::Path) {
    let mut queue: std::collections::VecDeque<update::Effect> = effects.into();
    while let Some(effect) = queue.pop_front() {
        let msg = exec::run_effect(effect, socket);
        for more in update::update(model, msg) {
            queue.push_back(more);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn a_key_press_maps_to_a_key_msg() {
        assert_eq!(
            map_event(press(KeyCode::Char('q'))),
            Some(update::Msg::Key(KeyCode::Char('q')))
        );
    }

    #[test]
    fn key_release_events_are_ignored() {
        // Some terminals emit Release too; acting on both double-fires every key.
        let release = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        assert_eq!(map_event(release), None);
    }

    #[test]
    fn non_key_events_are_ignored() {
        assert_eq!(map_event(Event::Resize(80, 24)), None);
    }
}
