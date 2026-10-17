//! `subcheck review`: accept, reject or reword each finding of a run, with a key that opens the
//! moment in mpv. A pure Elm-style core (`model`, `update`) with the run file and mpv at the edges
//! (`run_effect`), as in nudge-rs's dashboard.

pub mod model;
pub mod update;
pub mod view;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::Path;

use anyhow::Context;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::player::{self, Player};
use crate::scan::file_stamp;
use crate::store::{self, Record, Run, RunWriter};
use model::{FileNote, Model};
use update::{Effect, Msg};

/// A terminal event as a `Msg`. Ctrl and Alt chords never reach the plain-key handlers, so Ctrl-C
/// quits instead of reading as `c` (the lesson of nudge-rs's `map_event`).
pub fn map_event(ev: &Event) -> Option<Msg> {
    let Event::Key(k) = ev else {
        return None;
    };
    if k.kind != KeyEventKind::Press {
        return None;
    }
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        return match k.code {
            KeyCode::Char('c' | 'd') => Some(Msg::Quit),
            KeyCode::Char('u') => Some(Msg::KillLine),
            _ => None,
        };
    }
    if k.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    Some(Msg::Key(k.code))
}

/// How each file of the run differs from when it was scanned, by path; files as they were are
/// left out.
pub fn file_notes(run: &Run) -> HashMap<String, FileNote> {
    // Only files with findings are ever shown, and on a network share each check is a round trip.
    let shown: HashSet<&str> = run.findings.iter().map(|f| f.file.as_str()).collect();
    let mut notes = HashMap::new();
    // The newest record of each file wins: a file a run checked twice keeps its last stamp.
    for f in run.files.iter().filter(|f| shown.contains(f.path.as_str())) {
        let note = match file_stamp(Path::new(&f.path)) {
            Ok((size, mtime)) if size == f.size && mtime == f.mtime => None,
            Ok(_) => Some(FileNote::Changed),
            Err(_) => Some(FileNote::Missing),
        };
        match note {
            Some(n) => notes.insert(f.path.clone(), n),
            None => notes.remove(&f.path),
        };
    }
    notes
}

/// Run one effect and say how it went.
pub fn run_effect(effect: Effect, writer: &mut RunWriter, player: &mut Player) -> Msg {
    match effect {
        Effect::Write { action, decisions } => {
            let records: Vec<Record> = decisions.iter().cloned().map(Record::Decision).collect();
            let result = writer.append(&records).map_err(|e| e.to_string());
            Msg::Written {
                action,
                decisions,
                result,
            }
        }
        Effect::Open(moment) => Msg::Opened(player.open(&moment)),
    }
}

/// Restores the terminal on every exit path: normal return, `?`, or panic.
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

/// Review the run in `path`. `state` is the state directory, where mpv's socket goes when there is
/// no runtime directory.
pub fn run(path: &Path, state: &Path) -> anyhow::Result<()> {
    let run = store::read_run(path).with_context(|| format!("reading {}", path.display()))?;
    if run.findings.is_empty() {
        println!("No findings in run {}.", run.name);
        return Ok(());
    }
    let bad_lines = run.bad_lines;
    let notes = file_notes(&run);
    let mut writer =
        RunWriter::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut player = Player::new("mpv", player::socket_path(state));
    let mut model = Model::new(run, notes);
    if bad_lines > 0 {
        model.status = Some(format!(
            "{bad_lines} line(s) of the run file could not be read"
        ));
    }

    // Restore the terminal even if a panic unwinds through the loop.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        prev_hook(info);
    }));
    let guard = TerminalGuard::enter().context("entering the alternate screen")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    while !model.should_quit {
        terminal.draw(|f| view::view(&model, f))?;
        if let Some(msg) = map_event(&event::read()?) {
            let effects = update::update(&mut model, msg, jiff::Timestamp::now());
            // Show what is about to happen (`opening E02 at …`) before an effect holds the loop:
            // starting mpv can take seconds.
            if !effects.is_empty() {
                terminal.draw(|f| view::view(&model, f))?;
            }
            let mut queue: VecDeque<Effect> = effects.into();
            while let Some(effect) = queue.pop_front() {
                let msg = run_effect(effect, &mut writer, &mut player);
                queue.extend(update::update(&mut model, msg, jiff::Timestamp::now()));
            }
        }
    }
    drop(terminal);
    drop(guard);
    let (pending, accepted, rejected) = model.counts();
    println!(
        "run {}: {accepted} accepted, {rejected} rejected, {pending} pending",
        model.run.name
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    #[test]
    fn keys_map_to_msgs_and_chords_do_not_leak() {
        let key = |code, mods| Event::Key(KeyEvent::new(code, mods));
        assert_eq!(
            map_event(&key(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(Msg::Key(KeyCode::Char('a')))
        );
        assert_eq!(
            map_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Msg::Quit)
        );
        assert_eq!(
            map_event(&key(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            Some(Msg::KillLine)
        );
        // Ctrl-A is not `a`: it must not accept.
        assert_eq!(
            map_event(&key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(map_event(&key(KeyCode::Char('a'), KeyModifiers::ALT)), None);
        let release = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        assert_eq!(map_event(&release), None);
        assert_eq!(map_event(&Event::Resize(80, 24)), None);
    }

    #[test]
    fn files_that_changed_or_went_missing_are_noted() {
        let dir = tempfile::tempdir().unwrap();
        let same = dir.path().join("same.mkv");
        let changed = dir.path().join("changed.mkv");
        std::fs::write(&same, b"1234").unwrap();
        std::fs::write(&changed, b"1234").unwrap();
        let record = |p: &Path| {
            let (size, mtime) = file_stamp(p).unwrap();
            store::FileRecord {
                path: p.to_string_lossy().into_owned(),
                size,
                mtime,
                error: None,
                tracks: vec![],
                lines: 0,
                truncated: None,
            }
        };
        let gone = |name: &str| store::FileRecord {
            path: dir.path().join(name).to_string_lossy().into_owned(),
            ..record(&same)
        };
        let files = vec![
            record(&same),
            record(&changed),
            gone("gone.mkv"),
            gone("clean-and-gone.mkv"),
        ];
        // Findings in the first three; the fourth had none, so it is never shown or checked.
        let findings = files[..3]
            .iter()
            .map(|f| store::Finding {
                file: f.path.clone(),
                ..model::tests_support::finding(1, 1, "a", "b", 0)
            })
            .collect();
        let run = Run {
            files,
            findings,
            ..Run::default()
        };
        std::fs::write(&changed, b"12345").unwrap();
        let notes = file_notes(&run);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(notes[&run.files[1].path], FileNote::Changed);
        assert_eq!(notes[&run.files[2].path], FileNote::Missing);
    }
}
