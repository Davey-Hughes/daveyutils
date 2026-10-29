//! `subcheck review`: accept, reject or reword each finding of a run, with a key that opens the
//! moment in mpv. A pure Elm-style core (`model`, `update`) with the run file and mpv at the edges
//! (`run_effect`), as in nudge-rs's dashboard.

pub mod files;
pub mod jobs;
pub mod layout;
pub mod model;
pub mod progress;
pub mod update;
pub mod view;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use crate::group;
use crate::list;
use crate::player::{self, Player};
use crate::scan::{self, file_stamp};
use crate::store::{self, Record, Run, RunWriter};
use model::{FileNote, Model};
use progress::Scan;
use update::{Effect, Mouse, Msg};

/// A terminal event as a `Msg`. Ctrl and Alt chords never reach the plain-key handlers, so Ctrl-C
/// quits instead of reading as `c` (the lesson of nudge-rs's `map_event`); with the mouse they are
/// ignored.
pub fn map_event(ev: &Event) -> Option<Msg> {
    let k = match ev {
        Event::Key(k) => k,
        Event::Mouse(me) => {
            if me
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                return None;
            }
            let action = match me.kind {
                MouseEventKind::Down(MouseButton::Left) => Mouse::Click,
                MouseEventKind::ScrollUp => Mouse::WheelUp,
                MouseEventKind::ScrollDown => Mouse::WheelDown,
                _ => return None,
            };
            return Some(Msg::Mouse(action, me.column, me.row));
        }
        Event::Resize(w, h) => return Some(Msg::Resize(*w, *h)),
        _ => return None,
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

/// The next event from `read` that is a `Msg`. With the mouse captured, every movement of the
/// pointer is an event too; reading past those here saves a redraw for each.
fn next_msg(mut read: impl FnMut() -> io::Result<Event>) -> io::Result<Msg> {
    loop {
        if let Some(msg) = map_event(&read()?) {
            return Ok(msg);
        }
    }
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

/// How the command that finishes a scan names its run (`name`, at `path`) to `--run`: by its name
/// when `find_run` finds that run by it, as it does a run in the state directory `state`; else by
/// its path, which `--run` takes too, quoted for the shell. A run given by a path elsewhere has a
/// name that finds nothing, or another run.
fn run_arg(state: &Path, name: &str, path: &Path) -> String {
    let same = |found: PathBuf| match (std::fs::canonicalize(found), std::fs::canonicalize(path)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if store::find_run(state, Some(name)).is_ok_and(same) {
        name.to_string()
    } else {
        list::shell_quote(&path.to_string_lossy())
    }
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
        Effect::Playback(p) => Msg::Played(player.playback(p)),
    }
}

/// Restores the terminal on every exit path: normal return, `?`, or panic.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<TerminalGuard> {
        enable_raw_mode()?;
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture) {
            leave();
            return Err(e);
        }
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        leave();
    }
}

/// The terminal as it was: no raw mode, no mouse capture, the main screen.
fn leave() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
}

/// What review's screen opens on.
#[allow(clippy::large_enum_variant)] // one per process, moved once: a box would save nothing
pub enum Open {
    /// A run, as `subcheck review` names it.
    Run(PathBuf),
    /// A scan, as `subcheck scan` planned it: the screen runs it, on the run it writes to. `paths`
    /// are the arguments it was given, for the command that finishes it if it is stopped;
    /// `warnings`, what planning it wrote to the terminal (see [`Tee`]), which the screen covers.
    Scan {
        plan: scan::Plan,
        opts: scan::Options,
        paths: Vec<PathBuf>,
        warnings: Vec<String>,
    },
}

/// A log passed on to `inner` as it is written, and kept line by line: what `scan` warns of before
/// its screen opens stays on the terminal, and the screen, which covers it, can say so too.
pub struct Tee<W> {
    inner: W,
    /// What is written after the last newline.
    partial: Vec<u8>,
    lines: Vec<String>,
}

impl<W: Write> Tee<W> {
    pub fn new(inner: W) -> Tee<W> {
        Tee {
            inner,
            partial: Vec::new(),
            lines: Vec::new(),
        }
    }

    /// Each whole line written, without its newline.
    pub fn into_lines(self) -> Vec<String> {
        self.lines
    }
}

impl<W: Write> Write for Tee<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Kept as far as `inner` took it, so the two never differ.
        let n = self.inner.write(buf)?;
        self.partial.extend_from_slice(&buf[..n]);
        while let Some(end) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            self.lines
                .push(String::from_utf8_lossy(&line[..end]).into_owned());
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// What the status line opens with: the run file's lines that could not be read, else the first
/// of what `scan` warned of before the screen covered it; and how many warnings are more. The
/// unreadable lines lead because nothing else says them, while the warnings are printed above the
/// screen too, so `+n more` points at what the user can scroll up to. A message as others are,
/// until the next key.
fn opening_status(warnings: &[String], bad_lines: usize) -> Option<String> {
    let (first, more) = if bad_lines > 0 {
        let bad = format!("{bad_lines} line(s) of the run file could not be read");
        (bad, warnings.len())
    } else {
        let (first, rest) = warnings.split_first()?;
        (first.clone(), rest.len())
    };
    Some(match more {
        0 => first,
        n => format!("{first} · +{n} more"),
    })
}

/// How long the loop waits for a message while the scan runs before the spinner turns.
const TICK: Duration = Duration::from_millis(250);

/// Open the screen on `open`. `state` is the state directory, where mpv's socket goes when there
/// is no runtime directory. Exit status 1 when the scan failed, or a file in it did.
pub fn run(open: Open, state: &Path) -> anyhow::Result<ExitCode> {
    let (path, scanning, warnings) = match open {
        Open::Run(path) => (path, None, Vec::new()),
        Open::Scan {
            plan,
            opts,
            paths,
            warnings,
        } => (
            plan.run_path().to_path_buf(),
            Some((plan, opts, paths)),
            warnings,
        ),
    };
    let run = store::read_run(&path).with_context(|| format!("reading {}", path.display()))?;
    if run.findings.is_empty() && scanning.is_none() {
        println!("No findings in run {}.", run.name);
        return Ok(ExitCode::SUCCESS);
    }
    let bad_lines = run.bad_lines;
    let notes = file_notes(&run);
    let mut writer =
        RunWriter::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let mut player = Player::new("mpv", player::socket_path(state));
    // Else the status line opens on what the run's LLM calls cost (`Model::show_cost`).
    let mut model = Model::new(run, notes);
    model.status = opening_status(&warnings, bad_lines);
    let resume = scanning.as_ref().map(|(_, _, paths)| {
        progress::resume_command(&run_arg(state, &model.run.name, &path), paths)
    });

    // The terminal is restored should the screen's own thread panic. A job's panic comes to the
    // loop as `Msg::Fatal`, which ends the screen in order.
    let screen = std::thread::current().id();
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == screen {
            leave();
        }
        prev_hook(info);
    }));
    let guard = TerminalGuard::enter().context("entering the alternate screen")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let size = terminal.size()?;
    model.set_area(Rect::new(0, 0, size.width, size.height));
    let (tx, rx) = mpsc::channel();
    if let Some((plan, opts, _)) = scanning {
        model.progress.adding = opts.into.is_some();
        // Running from the start, so a `q` before the scan's first word still asks.
        model.progress.scan = Scan::Running;
        jobs::scan(plan, opts, state.to_path_buf(), tx.clone()).context("starting the scan")?;
    }
    // A scan may be running from here, so every way out goes through the teardown below, which
    // ends its LLM calls: an input thread that does not start, the loop's error, and a panic on
    // this thread, caught here (the hook has restored the terminal) and resumed once they are
    // ended. Not a drop guard: one made here would end the groups before `player`, made earlier,
    // let go of its own.
    let looped = match jobs::input(tx) {
        Ok(()) => panic::catch_unwind(AssertUnwindSafe(|| {
            screen_loop(&mut terminal, &mut model, &rx, &mut writer, &mut player)
        })),
        Err(e) => Ok(Err(e).context("reading the terminal")),
    };
    drop(terminal);
    drop(guard);
    // The mpv this review started is closed, and its registration let go, before `group::end`: one
    // let go after it would wait forever.
    drop(player);
    if model.progress.running() || model.fatal.is_some() {
        group::end();
    }
    let looped = looped.unwrap_or_else(|panic| panic::resume_unwind(panic));
    outcome(
        &mut model,
        looped,
        resume.as_deref().unwrap_or_default(),
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
}

/// What the terminal gets as the screen ends, and the exit status. On an error (the loop's, or the
/// model's fatal one), the error, returned; a scan it stopped says first how far it got and the
/// command that finishes it (`resume`). Otherwise the outro on `err`, then the decisions' counts
/// on `out`: exit status 1 when the scan failed, or a file in it did.
fn outcome(
    model: &mut Model,
    looped: anyhow::Result<()>,
    resume: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> anyhow::Result<ExitCode> {
    let outro = model.progress.outro(&model.run.name, resume);
    if let Some(e) = looped
        .err()
        .or_else(|| model.fatal.take().map(anyhow::Error::msg))
    {
        if model.progress.running() {
            for line in outro {
                writeln!(err, "{line}")?;
            }
        }
        return Err(e);
    }
    for line in outro {
        writeln!(err, "{line}")?;
    }
    let (pending, accepted, rejected) = model.counts();
    writeln!(
        out,
        "run {}: {accepted} accepted, {rejected} rejected, {pending} pending",
        model.run.name
    )?;
    let failed = model.progress.failed > 0 || matches!(model.progress.scan, Scan::Ended(Err(_)));
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Draw, take the next message (the terminal's, the scan's, or a tick while it runs), update, and
/// run the effects, until the model quits.
fn screen_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    model: &mut Model,
    rx: &mpsc::Receiver<Msg>,
    writer: &mut RunWriter,
    player: &mut Player,
) -> anyhow::Result<()> {
    while !model.should_quit {
        terminal.draw(|f| view::view(model, f))?;
        let msg = receive(rx, model.progress.running());
        let effects = update::update(model, msg, jiff::Timestamp::now());
        // Show what is about to happen (`opening E02 at …`) before an effect holds the loop:
        // starting mpv can take seconds.
        if !effects.is_empty() {
            terminal.draw(|f| view::view(model, f))?;
        }
        let mut queue: VecDeque<Effect> = effects.into();
        while let Some(effect) = queue.pop_front() {
            let msg = run_effect(effect, writer, player);
            queue.extend(update::update(model, msg, jiff::Timestamp::now()));
        }
    }
    Ok(())
}

/// The loop's next message: the terminal's or the scan's, or while the scan runs a tick should
/// none come for [`TICK`]. A channel no job sends on any more is fatal: each job says why it ends
/// before it does, so one went without a word, and with the input thread gone no key can quit.
fn receive(rx: &mpsc::Receiver<Msg>, running: bool) -> Msg {
    let got = if running {
        match rx.recv_timeout(TICK) {
            Err(mpsc::RecvTimeoutError::Timeout) => return Msg::Tick,
            got => got.ok(),
        }
    } else {
        rx.recv().ok()
    };
    got.unwrap_or_else(|| Msg::Fatal("the screen's input stopped".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, MouseEvent};

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
        assert_eq!(map_event(&Event::Resize(80, 24)), Some(Msg::Resize(80, 24)));
        assert_eq!(map_event(&Event::FocusGained), None);
    }

    #[test]
    fn events_that_mean_nothing_are_read_past_without_a_redraw() {
        let moved = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 7,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        let j = Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        let mut events = vec![moved.clone(), moved, Event::FocusGained, j].into_iter();
        let mut reads = 0;
        let msg = next_msg(|| {
            reads += 1;
            Ok(events.next().unwrap())
        });
        assert_eq!(msg.unwrap(), Msg::Key(KeyCode::Char('j')));
        assert_eq!(reads, 4);
    }

    #[test]
    fn left_clicks_and_the_wheel_map_to_msgs() {
        let mouse = |kind, modifiers| {
            Event::Mouse(MouseEvent {
                kind,
                column: 7,
                row: 3,
                modifiers,
            })
        };
        let none = KeyModifiers::NONE;
        assert_eq!(
            map_event(&mouse(MouseEventKind::Down(MouseButton::Left), none)),
            Some(Msg::Mouse(Mouse::Click, 7, 3))
        );
        assert_eq!(
            map_event(&mouse(MouseEventKind::ScrollUp, none)),
            Some(Msg::Mouse(Mouse::WheelUp, 7, 3))
        );
        assert_eq!(
            map_event(&mouse(MouseEventKind::ScrollDown, none)),
            Some(Msg::Mouse(Mouse::WheelDown, 7, 3))
        );
        for kind in [
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Moved,
        ] {
            assert_eq!(map_event(&mouse(kind, none)), None, "{kind:?}");
        }
        // Ctrl+wheel zooms the terminal; it is not a scroll.
        assert_eq!(
            map_event(&mouse(MouseEventKind::ScrollUp, KeyModifiers::CONTROL)),
            None
        );
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
                checks: vec![store::Check::Llm],
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

    #[test]
    fn a_scans_screen_opens_with_its_warnings_on_the_status_line() {
        let status = |m: &Model| m.status_text(Rect::new(0, 0, 160, 1));
        let warnings = vec![
            "warning: /m/locked: skipped: Permission denied (os error 13)".to_string(),
            "/m/E02.mkv: changed since run r; scan it into a new run".to_string(),
        ];
        let mut m = scanning();
        m.status = opening_status(&warnings, 0);
        assert_eq!(
            status(&m).as_deref(),
            Some("warning: /m/locked: skipped: Permission denied (os error 13) · +1 more")
        );
        // A message as others are: the next key ends it, and the scan's progress shows.
        update::update(&mut m, Msg::Key(KeyCode::Char('j')), jiff::Timestamp::now());
        assert_eq!(status(&m).as_deref(), Some("scan 0/2 · 0 findings"));
        // One warning alone; and the run file's unread lines, ahead of the warnings, or alone as
        // `review` opens with them.
        let one = || warnings[..1].to_vec();
        assert_eq!(opening_status(&one(), 0), Some(warnings[0].clone()));
        assert_eq!(
            opening_status(&one(), 2),
            Some("2 line(s) of the run file could not be read · +1 more".to_string())
        );
        assert_eq!(
            opening_status(&[], 2).as_deref(),
            Some("2 line(s) of the run file could not be read")
        );
        // The unreadable lines lead, since nothing else says them: `+n more` counts the warnings,
        // which are above the screen too.
        assert_eq!(
            opening_status(&warnings, 2),
            Some("2 line(s) of the run file could not be read · +2 more".to_string())
        );
        assert_eq!(opening_status(&[], 0), None);
    }

    #[test]
    fn a_tee_passes_the_log_on_and_keeps_each_whole_line() {
        let mut tee = Tee::new(Vec::new());
        write!(tee, "warning: a").unwrap();
        writeln!(tee, " b").unwrap();
        tee.write_all(b"c\nd").unwrap();
        assert_eq!(tee.inner, b"warning: a b\nc\nd");
        assert_eq!(tee.into_lines(), ["warning: a b", "c"]);
    }

    #[test]
    fn the_command_that_finishes_a_scan_names_a_run_elsewhere_by_its_path() {
        let state = tempfile::tempdir().unwrap();
        let runs = state.path().join("runs");
        std::fs::create_dir(&runs).unwrap();
        let name = "2026-10-17T04-39-18";
        let mine = runs.join(format!("{name}.jsonl"));
        std::fs::write(&mine, "").unwrap();
        // A run in the state directory goes by its name, however its path was given.
        assert_eq!(run_arg(state.path(), name, &mine), name);
        let roundabout = runs.join(format!("../runs/{name}.jsonl"));
        assert_eq!(run_arg(state.path(), name, &roundabout), name);
        // One elsewhere goes by its path: its name finds no run, or another run of that name.
        let elsewhere = tempfile::tempdir().unwrap();
        let x = elsewhere.path().join("x y.jsonl");
        std::fs::write(&x, "").unwrap();
        let quoted = list::shell_quote(&x.to_string_lossy());
        assert_eq!(run_arg(state.path(), "x y", &x), quoted);
        std::fs::write(runs.join("x y.jsonl"), "").unwrap();
        assert_eq!(run_arg(state.path(), "x y", &x), quoted);
    }

    #[test]
    fn a_channel_no_job_sends_on_any_more_ends_the_screen() {
        let (tx, rx) = mpsc::channel();
        tx.send(Msg::Key(KeyCode::Char('j'))).unwrap();
        drop(tx);
        assert_eq!(receive(&rx, true), Msg::Key(KeyCode::Char('j')));
        // Every job gone without a word: whether the scan runs or not, the screen cannot go on.
        for running in [true, false] {
            assert_eq!(
                receive(&rx, running),
                Msg::Fatal("the screen's input stopped".into())
            );
        }
        let (_tx, rx) = mpsc::channel();
        assert_eq!(receive(&rx, true), Msg::Tick);
    }

    /// The sample run with a scan of two files under way, as the loop leaves it on a stop.
    fn scanning() -> Model {
        let mut m = model::tests_support::sample();
        m.progress.scan_event(&scan::Event::Queued(vec![
            "/m/E01.mkv".into(),
            "/m/E02.mkv".into(),
        ]));
        m
    }

    /// What `outcome` returns, and what it wrote to stdout and stderr.
    fn ended(
        m: &mut Model,
        looped: anyhow::Result<()>,
    ) -> (anyhow::Result<ExitCode>, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = outcome(m, looped, "subcheck scan --run r /m", &mut out, &mut err);
        let text = |b: Vec<u8>| String::from_utf8(b).unwrap();
        (code, text(out), text(err))
    }

    #[test]
    fn an_error_that_stops_the_scan_says_first_how_to_finish_it() {
        let stopped = "run 2026-10-17T04-39-18: stopped at 0/2 files, 0 failed; 0 findings\n\
                       Finish it with: subcheck scan --run r /m\n";
        let mut m = scanning();
        m.fatal = Some("the scan panicked: boom".into());
        let (code, out, err) = ended(&mut m, Ok(()));
        assert_eq!(code.unwrap_err().to_string(), "the scan panicked: boom");
        assert_eq!((out.as_str(), err.as_str()), ("", stopped));
        // The loop's own error: the terminal could not be drawn on, say.
        let (code, out, err) = ended(&mut scanning(), Err(anyhow::anyhow!("os error 5")));
        assert_eq!(code.unwrap_err().to_string(), "os error 5");
        assert_eq!((out.as_str(), err.as_str()), ("", stopped));
        // With no scan running, the error alone.
        let mut m = model::tests_support::sample();
        m.fatal = Some("reading the terminal: gone".into());
        let (code, out, err) = ended(&mut m, Ok(()));
        assert_eq!(code.unwrap_err().to_string(), "reading the terminal: gone");
        assert_eq!((out.as_str(), err.as_str()), ("", ""));
    }

    #[test]
    fn a_stop_alone_exits_0_and_a_failure_1() {
        let (code, out, err) = ended(&mut scanning(), Ok(()));
        assert_eq!(code.unwrap(), ExitCode::SUCCESS);
        assert!(
            err.contains("Finish it with: subcheck scan --run r /m"),
            "{err}"
        );
        assert_eq!(
            out,
            "run 2026-10-17T04-39-18: 0 accepted, 0 rejected, 4 pending\n"
        );
        let mut m = scanning();
        m.progress.scan_event(&scan::Event::Finished {
            path: "/m/E01.mkv".into(),
            error: Some("LLM: boom".into()),
            findings: 0,
            lines: vec![],
        });
        assert_eq!(ended(&mut m, Ok(())).0.unwrap(), ExitCode::FAILURE);
        let mut m = scanning();
        m.progress.scan = Scan::Ended(Err("disk full".into()));
        assert_eq!(ended(&mut m, Ok(())).0.unwrap(), ExitCode::FAILURE);
        let mut m = model::tests_support::sample();
        let (code, out, err) = ended(&mut m, Ok(()));
        assert_eq!(code.unwrap(), ExitCode::SUCCESS);
        assert_eq!(err, "");
        assert!(out.ends_with("4 pending\n"), "{out}");
    }
}
