//! Keys to state changes, and the effects the event loop runs for them. Pure: the store and mpv are
//! touched only by the effects, so every rule here is tested without a terminal.

use std::collections::HashMap;

use crossterm::event::KeyCode;
use jiff::{SignedDuration, Timestamp};
use ratatui::layout::{Position, Rect};

use super::layout;
use super::model::{Confirm, Editor, Entry, FileNote, Mark, Model, Pane, UndoStep};
use super::progress::{Scan, Stage};
use super::view;
use crate::apply;
use crate::list::{self, Filter, label};
use crate::player::{Moment, PlayState, Playback};
use crate::scan;
use crate::store::{Decided, Decision, Record, Run, State};
use crate::text::{timestamp, timestamp_ms};

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // a read-back is rare; boxing it would only add noise
pub enum Msg {
    Key(KeyCode),
    /// Ctrl-U: in the editor, delete back to the start.
    KillLine,
    Quit,
    /// A `Write` effect finished: what became of each decision, or why none was written.
    Written {
        action: Action,
        decisions: Vec<Decision>,
        result: Result<Vec<Decided>, String>,
    },
    /// A `ReadBack` effect finished: the file as the store shows it, or why it could not be read.
    ReadBack {
        path: String,
        view: Result<Run, String>,
    },
    /// A `FindRun` effect finished: the run's name and backend, or why there is none.
    RunFound {
        path: String,
        found: Result<(String, String), String>,
    },
    /// A `Recheck` effect finished: the scan it started, or why none did; the files it left out,
    /// no longer on disk; and all the files it was asked for (when no scan started, those still
    /// on disk fail with the reason).
    Started {
        job: Result<Job, String>,
        missing: Vec<String>,
        paths: Vec<String>,
    },
    /// A `Switch` effect finished: the view and its files' notes, or why it could not be read.
    Switched(Result<(Run, HashMap<String, FileNote>), String>),
    /// An `Open` effect finished.
    Opened(Result<(), String>),
    /// A `Playback` effect finished: where mpv is now.
    Played(Result<PlayState, String>),
    /// The terminal's new size.
    Resize(u16, u16),
    /// A mouse action at a column and row of the screen.
    Mouse(Mouse, u16, u16),
    /// What the scan under way just did.
    Scan(scan::Event),
    /// The scan ended: its summary, or why it failed.
    ScanEnded(Result<scan::Summary, String>),
    /// Something the screen cannot go on without failed (a job's thread panicked, the terminal
    /// could not be read): it ends, with this error.
    Fatal(String),
    /// A quarter of a second passed while the scan runs: the spinner turns.
    Tick,
    /// What the screen's apply (or dry run) just did.
    Apply(apply::Event),
    /// A `StartApply` effect finished: apply's thread started, or why it could not.
    ApplyStarted(Result<(), String>),
    /// Apply ended: its summary, or why it failed as a whole.
    ApplyEnded(Result<apply::Summary, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mouse {
    Click,
    WheelUp,
    WheelDown,
}

/// What a write was for, so its result can be applied.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// `a` `r` `e`, and `x`, which puts findings back to pending.
    Decide {
        entry: Entry,
        before: UndoStep,
    },
    Undo,
    /// New findings taking their rows' decisions (see [`inherit`]).
    Inherit {
        before: UndoStep,
    },
}

/// A scan `c` started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub run: String,
    /// The command that finishes it, should it stop.
    pub resume: String,
}

/// The view `v` opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Switch {
    /// The folders of the run of this name.
    FoldersOf(String),
    /// The newest run that named the file at this path.
    RunNaming(String),
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // effects are few and short-lived, so a box is not worth it
pub enum Effect {
    /// Record these decisions in the store.
    Write {
        action: Action,
        decisions: Vec<Decision>,
    },
    /// Read a file back from the store, as it is now ([`Msg::ReadBack`]).
    ReadBack(String),
    /// Find the run `c` checks a file again into: `run`, the one the screen shows, or with none (a
    /// paths view) the newest that named the file ([`Msg::RunFound`]).
    FindRun {
        path: String,
        run: Option<String>,
    },
    /// Check `paths` again into `run`, as `subcheck scan --run <run> --force <paths>` does
    /// ([`Msg::Started`]).
    Recheck {
        run: String,
        paths: Vec<String>,
    },
    /// Open the other view of the screen's findings (`v`), read from the store with each file's
    /// note ([`Msg::Switched`]).
    Switch(Switch),
    /// Apply `run`'s accepted fixes on a thread of their own (with `dry_run`, say what would change
    /// and write nothing); `karaoke` as `apply --karaoke` ([`Msg::ApplyStarted`], then
    /// [`Msg::Apply`] for each step and [`Msg::ApplyEnded`]).
    StartApply {
        run: Run,
        dry_run: bool,
        karaoke: bool,
    },
    Open(Moment),
    /// Pause or seek the open mpv.
    Playback(Playback),
}

/// Rows a PgUp or PgDn moves.
const PAGE: usize = 10;

/// Two clicks on one entry within this long are a double-click.
const DOUBLE_CLICK: SignedDuration = SignedDuration::from_millis(500);

/// Lines the detail pane scrolls per notch of the wheel.
const WHEEL_LINES: u16 = 3;

pub fn update(m: &mut Model, msg: Msg, now: Timestamp) -> Vec<Effect> {
    let before = (m.selected_entry(), m.pane, m.file_selected);
    let input = matches!(
        msg,
        Msg::Key(_) | Msg::KillLine | Msg::Quit | Msg::Mouse(..)
    );
    // A double-click is two clicks with nothing the user did in between: a key or another click
    // starts over. The scan's messages and the spinner's ticks come unasked, so they leave the
    // first click be (`Model::refresh` keeps it on its entry as rows come and go).
    let last_click = if input { m.last_click.take() } else { None };
    let effects = apply(m, msg, last_click, now);
    // The cost line review opened with ends at the first key (Ctrl-U and Ctrl-C too), click or
    // turn of the wheel, the box of keys open or not: once the message is handled, so a click
    // lands on the status line as it was drawn.
    if input {
        m.show_cost = false;
    }
    if (m.selected_entry(), m.pane, m.file_selected) != before {
        m.detail_scroll = 0;
    }
    // No further than the detail's last line, however that moved: the wheel, a wider screen, a
    // decision or a filter that shortened the text.
    m.detail_scroll = m.detail_scroll.min(detail_last(m));
    // The list scrolls no further than it takes to keep the selection in view.
    m.scroll = m.list_scroll(m.area);
    m.file_scroll = m.files_scroll(m.area);
    effects
}

#[allow(clippy::too_many_lines)] // one arm per message
fn apply(
    m: &mut Model,
    msg: Msg,
    last_click: Option<(Entry, Timestamp)>,
    now: Timestamp,
) -> Vec<Effect> {
    match msg {
        // Ctrl-C or Ctrl-D: while a job runs it asks first, as `q` does; a second one, with the
        // question open, quits.
        Msg::Quit => {
            if m.progress.busy() && m.confirm != Some(Confirm::Quit) {
                m.confirm = Some(Confirm::Quit);
            } else {
                m.should_quit = true;
            }
            Vec::new()
        }
        Msg::KillLine => {
            if let Some(ed) = &mut m.editor {
                ed.text = ed.text.chars().skip(ed.cursor).collect();
                ed.cursor = 0;
            }
            Vec::new()
        }
        // The question takes the next key: `y` answers it, any other lets it go.
        Msg::Key(key) if m.confirm.is_some() => {
            let confirm = m.confirm.take().expect("checked above");
            if key != KeyCode::Char('y') {
                return Vec::new();
            }
            match confirm {
                Confirm::Quit => {
                    m.should_quit = true;
                    Vec::new()
                }
                Confirm::Recheck { path, run, .. } => again(m, path, run),
                Confirm::Apply => start_apply(m, false),
            }
        }
        Msg::Key(key) if m.editor.is_some() => edit_key(m, key, now),
        // Any key closes the box of keys and does nothing else; but `q` quits, as the box says.
        Msg::Key(key) if m.help => {
            m.help = false;
            if key == KeyCode::Char('q') {
                quit_or_ask(m);
            }
            Vec::new()
        }
        Msg::Key(key) => {
            m.status = None;
            match m.pane {
                Pane::Findings => key_press(m, key, now),
                Pane::Files => files_key(m, key, now),
            }
        }
        Msg::Written {
            action,
            decisions,
            result,
        } => written(m, action, &decisions, result),
        Msg::ReadBack { path, view } => read_back(m, &path, view, now),
        Msg::RunFound { path, found } => run_found(m, path, found),
        Msg::Started {
            job,
            missing,
            paths: p,
        } => started(m, job, &missing, &p),
        Msg::Switched(result) => switched(m, result),
        Msg::Opened(result) => {
            m.status = Some(match result {
                Ok(()) => "opened in mpv".into(),
                Err(e) => e,
            });
            Vec::new()
        }
        Msg::Played(result) => {
            m.status = Some(match result {
                Ok(p) => format!(
                    "{} {}",
                    if p.paused { "⏸" } else { "▶" },
                    timestamp_ms(p.at_ms)
                ),
                Err(e) => e,
            });
            Vec::new()
        }
        Msg::Resize(width, height) => {
            m.set_area(Rect::new(0, 0, width, height));
            Vec::new()
        }
        Msg::Mouse(action, x, y) => mouse(m, action, Position::new(x, y), last_click, now),
        Msg::Scan(event) => scanned(m, event),
        Msg::ScanEnded(result) => {
            m.progress.scan = Scan::Ended(result);
            let effects = next_again(m);
            settle_quit(m, &effects);
            effects
        }
        Msg::Apply(event) => applied(m, event),
        Msg::ApplyStarted(Ok(())) => Vec::new(),
        Msg::ApplyStarted(Err(e)) | Msg::ApplyEnded(Err(e)) => {
            m.progress.apply_ended(Err(e));
            settle_quit(m, &[]);
            Vec::new()
        }
        Msg::ApplyEnded(Ok(summary)) => {
            m.progress.apply_ended(Ok(summary));
            settle_quit(m, &[]);
            Vec::new()
        }
        Msg::Fatal(error) => {
            m.fatal = Some(error);
            m.should_quit = true;
            Vec::new()
        }
        Msg::Tick => {
            m.progress.ticks += 1;
            Vec::new()
        }
    }
}

/// The furthest the detail pane scrolls: its last line, wrapped as drawn, on top.
fn detail_last(m: &Model) -> u16 {
    // Inside the border, where the lines wrap.
    let width = layout::panes(m.area).detail.width.saturating_sub(2);
    let lines = view::detail_height(m, width);
    u16::try_from(lines.saturating_sub(1)).unwrap_or(u16::MAX)
}

/// A click does what its key does; the wheel moves the selection over the list and scrolls the
/// detail pane. While the editor is open only its own buttons work, so a stray click cannot lose
/// the edit; while the box of keys shows, a click closes it and the wheel does nothing.
fn mouse(
    m: &mut Model,
    action: Mouse,
    at: Position,
    last_click: Option<(Entry, Timestamp)>,
    now: Timestamp,
) -> Vec<Effect> {
    let p = layout::panes(m.area);
    let press = |m: &mut Model, key: Option<KeyCode>| match key {
        Some(key) => apply(m, Msg::Key(key), None, now),
        None => Vec::new(),
    };
    if m.help {
        m.help = !matches!(action, Mouse::Click);
        return Vec::new();
    }
    if m.editor.is_some() {
        return match action {
            Mouse::Click => press(m, status_button(m, p.status, at)),
            _ => Vec::new(),
        };
    }
    match action {
        Mouse::WheelUp if p.list.contains(at) => press(m, Some(KeyCode::Up)),
        Mouse::WheelDown if p.list.contains(at) => press(m, Some(KeyCode::Down)),
        Mouse::WheelUp if p.detail.contains(at) => {
            m.detail_scroll = m.detail_scroll.saturating_sub(WHEEL_LINES);
            Vec::new()
        }
        Mouse::WheelDown if p.detail.contains(at) => {
            // `update` stops it at the last line.
            m.detail_scroll = m.detail_scroll.saturating_add(WHEEL_LINES);
            Vec::new()
        }
        Mouse::Click if layout::list_rows(&p).contains(at) => {
            let row = usize::from(at.y - layout::list_rows(&p).y);
            match m.pane {
                Pane::Findings => click_row(m, m.scroll + row, last_click, now),
                Pane::Files => {
                    if m.file_scroll + row < m.progress.files.len() {
                        m.status = None;
                        m.file_selected = m.file_scroll + row;
                    }
                    Vec::new()
                }
            }
        }
        Mouse::Click => press(m, status_button(m, p.status, at)),
        _ => Vec::new(),
    }
}

/// The key the button at `at` on the status line `status` presses, while the buttons show.
fn status_button(m: &Model, status: Rect, at: Position) -> Option<KeyCode> {
    let items = m.buttons(status).filter(|_| status.contains(at))?;
    layout::bar_hit(items, layout::bar_left(status, items), at.x)
}

/// A click on the list's `index`th entry selects it; the second click of a double-click opens it.
fn click_row(
    m: &mut Model,
    index: usize,
    last_click: Option<(Entry, Timestamp)>,
    now: Timestamp,
) -> Vec<Effect> {
    let Some(&entry) = m.entries().get(index) else {
        return Vec::new();
    };
    m.status = None;
    m.selected = index;
    // The wall clock can step back between two clicks: a negative gap is not a short one.
    let double = last_click.is_some_and(|(e, at)| {
        e == entry && (SignedDuration::ZERO..=DOUBLE_CLICK).contains(&now.duration_since(at))
    });
    if double {
        return open(m);
    }
    m.last_click = Some((entry, now));
    Vec::new()
}

fn key_press(m: &mut Model, key: KeyCode, now: Timestamp) -> Vec<Effect> {
    let last = m.entries().len().saturating_sub(1);
    match key {
        KeyCode::Char('q') => quit_or_ask(m),
        KeyCode::Tab => m.pane = Pane::Files,
        KeyCode::Char('v') => return switch_view(m),
        KeyCode::Char('A') => return apply_key(m, false),
        KeyCode::Char('D') => return apply_key(m, true),
        KeyCode::Char('?') => m.help = true,
        KeyCode::Down | KeyCode::Char('j') => m.selected = (m.selected + 1).min(last),
        KeyCode::Up | KeyCode::Char('k') => m.selected = m.selected.saturating_sub(1),
        KeyCode::PageDown => m.selected = (m.selected + PAGE).min(last),
        KeyCode::PageUp => m.selected = m.selected.saturating_sub(PAGE),
        KeyCode::Home | KeyCode::Char('g') => m.selected = 0,
        KeyCode::End | KeyCode::Char('G') => m.selected = last,
        KeyCode::Char('f') => cycle_filter(m),
        KeyCode::Enter => toggle(m),
        KeyCode::Char('n') => next_file(m),
        KeyCode::Char('p') => previous_file(m),
        KeyCode::Char('a') => {
            if let Some(entry) = m.selected_entry() {
                return decide(m, entry, State::Accepted, None, now);
            }
        }
        KeyCode::Char('r') => {
            if let Some(entry) = m.selected_entry() {
                return decide(m, entry, State::Rejected, None, now);
            }
        }
        KeyCode::Char('e') => {
            if let Some(entry) = m.selected_entry() {
                let targets = m.to_decide(entry);
                if targets.iter().any(|&i| m.finding(i).timing.is_some()) {
                    m.status = Some("a timing fix is accepted or rejected, not edited".into());
                    return Vec::new();
                }
                let text = m.wording(targets[0]).to_string();
                let cursor = text.chars().count();
                m.editor = Some(Editor {
                    entry,
                    text,
                    cursor,
                });
            }
        }
        KeyCode::Char('x') => {
            if let Some(entry) = m.selected_entry() {
                return clear(m, entry, now);
            }
        }
        KeyCode::Char('u') => return undo(m, now),
        KeyCode::Char('o') => return open(m),
        KeyCode::Char(' ') => return vec![Effect::Playback(Playback::Pause)],
        KeyCode::Left => return vec![Effect::Playback(Playback::Seek(-5))],
        KeyCode::Right => return vec![Effect::Playback(Playback::Seek(5))],
        KeyCode::Char(',') => return vec![Effect::Playback(Playback::Seek(-1))],
        KeyCode::Char('.') => return vec![Effect::Playback(Playback::Seek(1))],
        KeyCode::Char('[') => return vec![Effect::Playback(Playback::PreviousFrame)],
        KeyCode::Char(']') => return vec![Effect::Playback(Playback::NextFrame)],
        _ => {}
    }
    Vec::new()
}

fn edit_key(m: &mut Model, key: KeyCode, now: Timestamp) -> Vec<Effect> {
    let Some(ed) = &mut m.editor else {
        return Vec::new();
    };
    let at = |text: &str, cursor: usize| {
        text.char_indices()
            .nth(cursor)
            .map_or(text.len(), |(i, _)| i)
    };
    match key {
        KeyCode::Esc => m.editor = None,
        KeyCode::Enter => {
            let ed = m.editor.take().expect("checked above");
            if ed.text.trim().is_empty() {
                m.status = Some("the wording cannot be empty (Esc cancels)".into());
                m.editor = Some(ed);
                return Vec::new();
            }
            let reworded = ed.text != m.finding(m.to_decide(ed.entry)[0]).suggestion;
            let edited = reworded.then_some(ed.text.as_str());
            return decide(m, ed.entry, State::Accepted, edited, now);
        }
        KeyCode::Char(c) => {
            let i = at(&ed.text, ed.cursor);
            ed.text.insert(i, c);
            ed.cursor += 1;
        }
        KeyCode::Backspace if ed.cursor > 0 => {
            ed.cursor -= 1;
            let i = at(&ed.text, ed.cursor);
            ed.text.remove(i);
        }
        KeyCode::Delete if ed.cursor < ed.text.chars().count() => {
            let i = at(&ed.text, ed.cursor);
            ed.text.remove(i);
        }
        KeyCode::Left => ed.cursor = ed.cursor.saturating_sub(1),
        KeyCode::Right => ed.cursor = (ed.cursor + 1).min(ed.text.chars().count()),
        KeyCode::Home => ed.cursor = 0,
        KeyCode::End => ed.cursor = ed.text.chars().count(),
        _ => {}
    }
    Vec::new()
}

/// Decisions putting each of `findings` at `state` with that wording, but those that already are;
/// and what each had before, for `u`.
fn changes(
    m: &Model,
    findings: Vec<usize>,
    state: State,
    edited: Option<&str>,
    now: Timestamp,
) -> (UndoStep, Vec<Decision>) {
    let mut before = Vec::new();
    let mut decisions = Vec::new();
    for i in findings {
        let id = m.finding(i).id;
        let current = m.run.decisions.get(&id);
        if current.is_some_and(|d| d.state == state && d.edited.as_deref() == edited) {
            continue;
        }
        before.push((id, current.cloned()));
        decisions.push(Decision {
            finding: id,
            state,
            edited: edited.map(str::to_string),
            at: now,
        });
    }
    (before, decisions)
}

/// `a` `r` `e`: the findings `entry` decides ([`Model::to_decide`]) at `state`, with that wording.
fn decide(
    m: &mut Model,
    entry: Entry,
    state: State,
    edited: Option<&str>,
    now: Timestamp,
) -> Vec<Effect> {
    let (before, decisions) = changes(m, m.to_decide(entry), state, edited, now);
    if decisions.is_empty() {
        m.status = Some(format!("already {}", label(state)));
        return Vec::new();
    }
    vec![Effect::Write {
        action: Action::Decide { entry, before },
        decisions,
    }]
}

/// `x`: each finding `entry` stands for back to pending, but those that are.
fn clear(m: &mut Model, entry: Entry, now: Timestamp) -> Vec<Effect> {
    let decided: Vec<usize> = m
        .targets(entry)
        .into_iter()
        .filter(|&i| m.state(i) != State::Pending)
        .collect();
    let (before, decisions) = changes(m, decided, State::Pending, None, now);
    if decisions.is_empty() {
        m.status = Some("nothing to clear".into());
        return Vec::new();
    }
    vec![Effect::Write {
        action: Action::Decide { entry, before },
        decisions,
    }]
}

/// Decisions that put back what the last decision of this session changed.
fn undo(m: &mut Model, now: Timestamp) -> Vec<Effect> {
    let Some(step) = m.undo.last() else {
        m.status = Some("nothing to undo".into());
        return Vec::new();
    };
    let decisions = step
        .iter()
        .map(|(id, before)| Decision {
            finding: *id,
            state: before.as_ref().map_or(State::Pending, |d| d.state),
            edited: before.as_ref().and_then(|d| d.edited.clone()),
            at: now,
        })
        .collect();
    vec![Effect::Write {
        action: Action::Undo,
        decisions,
    }]
}

/// A write's result: on success the decisions take effect, and the selection moves on. A decision
/// the store found on a replaced finding went to its new one, or nowhere: its file is read back.
#[allow(clippy::too_many_lines)] // the write's every consequence for the view, in order
fn written(
    m: &mut Model,
    action: Action,
    decisions: &[Decision],
    result: Result<Vec<Decided>, String>,
) -> Vec<Effect> {
    let outcomes = match result {
        Ok(outcomes) => outcomes,
        Err(e) => {
            m.status = Some(format!("not saved: {e}"));
            return Vec::new();
        }
    };
    let old = m.selected;
    let shown = m.entries();
    for d in decisions {
        m.run.decisions.insert(d.finding, d.clone());
        // A dry run's mark said what apply would do with the decision as it was.
        if m.marks.get(&d.finding).is_some_and(Mark::dry_run) {
            m.marks.remove(&d.finding);
        }
    }
    let n = decisions.len();
    let what = if n == 1 {
        "1 finding".to_string()
    } else {
        format!("{n} findings")
    };
    match action {
        Action::Decide { entry, before } => {
            let state = decisions[0].state;
            m.status = Some(if state == State::Pending {
                format!("cleared {what}")
            } else {
                // `a` means the suggestion as written, so it replaces a rewording; say so, since
                // `u` can bring it back.
                let replaced = before
                    .iter()
                    .zip(decisions)
                    .filter(|((_, was), now)| {
                        now.state == State::Accepted
                            && now.edited.is_none()
                            && was.as_ref().is_some_and(|d| d.edited.is_some())
                    })
                    .count();
                let replacing = if replaced > 0 {
                    format!(", replacing your wording on {replaced} (u undoes)")
                } else {
                    String::new()
                };
                // The members a row's key left alone, decided the other way on their own.
                let kept: Vec<State> = m
                    .targets(entry)
                    .into_iter()
                    .map(|i| m.state(i))
                    .filter(|&s| s != state)
                    .collect();
                let kept = match kept.first() {
                    Some(&other) => format!(", {} kept as {}", kept.len(), label(other)),
                    None => String::new(),
                };
                format!("{} {what}{replacing}{kept}", label(state))
            });
            m.undo.push(before);
            advance(m, entry, old, &shown);
        }
        Action::Inherit { before } => {
            m.undo.push(before);
            // The decision can hide a row the new member showed under the filter, or show the
            // member under an expanded row: the selection stays on the entry it was on, wherever
            // that is in the list now.
            if let Some(at) = shown
                .get(old)
                .and_then(|e| m.entries().iter().position(|n| n == e))
            {
                // `Model::refresh` raised the scroll to hold the row's screen line; the hidden row
                // took a line from above it, so the scroll gives one back, and the row stays
                // where the user was looking.
                m.scroll = (m.scroll + at).saturating_sub(old);
                m.selected = at;
            }
            let first = &decisions[0];
            let file = m
                .run
                .findings
                .iter()
                .find(|f| f.id == first.finding)
                .map(|f| f.file.clone())
                .unwrap_or_default();
            let name = m.label(&file);
            m.status = Some(if n == 1 {
                format!(
                    "{name}: a new finding took its row's decision: {} (u undoes)",
                    label(first.state)
                )
            } else {
                format!("{name}: {n} new findings took their rows' decisions (u undoes)")
            });
        }
        Action::Undo => {
            m.undo.pop();
            // Name the episodes taken back, since the undone step may be a late join in a file
            // the user is not looking at.
            let names = episodes(m, decisions);
            m.status = Some(format!("undone: {what} · {names}"));
            // Select the row the undone decision was on, if the filter shows it.
            let first = m
                .run
                .findings
                .iter()
                .position(|f| f.id == decisions[0].finding);
            let at = m.entries().iter().position(|&e| match e {
                Entry::Row(r) => first.is_some_and(|i| m.rows[r].members.contains(&i)),
                Entry::Member(..) => false,
            });
            if let Some(at) = at {
                m.selected = at;
            }
        }
    }
    m.clamp();
    replaced(m, decisions, &outcomes)
}

/// The files whose findings a write found replaced: the store put each decision on its finding's
/// new one, or on none. Each is read back, and the status line says which happened.
fn replaced(m: &mut Model, decisions: &[Decision], outcomes: &[Decided]) -> Vec<Effect> {
    let mut files: Vec<String> = Vec::new();
    let mut gone = false;
    for (d, outcome) in decisions.iter().zip(outcomes) {
        if *outcome == Decided::Written {
            continue;
        }
        gone |= *outcome == Decided::Gone;
        if let Some(f) = m.run.findings.iter().find(|f| f.id == d.finding)
            && !files.contains(&f.file)
        {
            files.push(f.file.clone());
        }
    }
    let Some(first) = files.first() else {
        return Vec::new();
    };
    let what = if gone {
        "that finding is gone"
    } else {
        "your decision went to its new finding"
    };
    m.status = Some(format!("{} was checked again; {what}", m.label(first)));
    files.into_iter().map(Effect::ReadBack).collect()
}

/// The episode labels of the files the decisions are on, each once, in order of first appearance.
fn episodes(m: &Model, decisions: &[Decision]) -> String {
    let mut names: Vec<String> = Vec::new();
    for d in decisions {
        let file = m
            .run
            .findings
            .iter()
            .find(|f| f.id == d.finding)
            .map(|f| f.file.clone())
            .unwrap_or_default();
        let name = m.label(&file);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names.join(", ")
}

/// After a decision on `decided`, which was at `old` in `shown` (the list before it): select the
/// first entry after it that is still shown and has a pending finding, passing over a decided
/// row's members. Under a filter of accepted or rejected findings, where nothing shown is
/// pending, the first one still shown. The decided entry may be gone, and with it more than
/// itself: the last shown member of a group takes the group's row along.
fn advance(m: &mut Model, decided: Entry, old: usize, shown: &[Entry]) {
    let now = m.entries();
    let to_do = matches!(m.filter, Filter::All | Filter::Only(State::Pending));
    let next = shown
        .iter()
        .skip(old + 1)
        .filter(|e| {
            !(matches!(decided, Entry::Row(_))
                && matches!(e, Entry::Member(r, _) if *r == decided.row()))
        })
        .filter(|&&e| !to_do || m.has_pending(e))
        .find_map(|e| now.iter().position(|n| n == e));
    m.selected = match next {
        Some(i) => i,
        // Nothing after it: stay on it if it is still there, else on the last entry.
        None => now
            .iter()
            .position(|&e| e == decided)
            .unwrap_or(now.len().saturating_sub(1)),
    };
}

fn cycle_filter(m: &mut Model) {
    let focus = m.focus();
    m.filter = match m.filter {
        Filter::All => Filter::Only(State::Pending),
        Filter::Only(State::Pending) => Filter::Only(State::Accepted),
        Filter::Only(State::Accepted) => Filter::Only(State::Rejected),
        Filter::Only(State::Rejected) => Filter::All,
    };
    // Stay on the same finding's row when it is still shown.
    let entries = m.entries();
    m.selected = focus
        .and_then(|i| {
            entries
                .iter()
                .position(|&e| matches!(e, Entry::Row(r) if m.rows[r].members.contains(&i)))
        })
        .unwrap_or(0);
}

/// `⏎`: expand a group into its members, or collapse it (from the row or from a member).
fn toggle(m: &mut Model) {
    match m.selected_entry() {
        Some(Entry::Row(r)) if m.rows[r].members.len() > 1 => {
            if !m.expanded.remove(&r) {
                m.expanded.insert(r);
            }
        }
        Some(Entry::Member(r, _)) => {
            m.expanded.remove(&r);
            if let Some(at) = m.entries().iter().position(|&e| e == Entry::Row(r)) {
                m.selected = at;
            }
        }
        _ => {}
    }
}

fn next_file(m: &mut Model) {
    let entries = m.entries();
    let Some(current) = entries.get(m.selected).and_then(|&e| m.file_of(e)) else {
        return;
    };
    match (m.selected + 1..entries.len()).find(|&i| m.file_of(entries[i]) != Some(current)) {
        Some(i) => m.selected = i,
        None => m.status = Some("last file".into()),
    }
}

fn previous_file(m: &mut Model) {
    let entries = m.entries();
    let file = |i: usize| m.file_of(entries[i]);
    let Some(current) = entries.get(m.selected).and_then(|&e| m.file_of(e)) else {
        return;
    };
    // The nearest entry of another file, then the first entry of that file's run of entries.
    let Some(mut i) = (0..m.selected).rev().find(|&i| file(i) != Some(current)) else {
        m.status = Some("first file".into());
        return;
    };
    let previous = file(i);
    while i > 0 && file(i - 1) == previous {
        i -= 1;
    }
    m.selected = i;
}

fn open(m: &mut Model) -> Vec<Effect> {
    let Some(i) = m.focus() else {
        return Vec::new();
    };
    let f = m.finding(i);
    if m.notes.get(&f.file) == Some(&FileNote::Missing) {
        m.status = Some(format!("file missing: {}", f.file));
        return Vec::new();
    }
    // A timing finding opens on the frame that shows its error, paused; a line two seconds early,
    // playing.
    let (start_ms, sid, paused) = if let Some(at) = list::timing_moment(&m.run, f) {
        (at.at_ms, at.sid, true)
    } else if f.timing.is_some() {
        m.status = Some("nothing to open: no line of this track moves".into());
        return Vec::new();
    } else {
        let (at, sid) = list::mpv_moment(&m.run, f);
        (at, sid, false)
    };
    let moment = Moment {
        file: f.file.clone(),
        start_ms,
        sid,
        paused,
    };
    m.status = Some(if paused {
        format!(
            "opening {} at {}, paused",
            m.label(&moment.file),
            timestamp_ms(start_ms)
        )
    } else {
        format!(
            "opening {} at {}",
            m.label(&moment.file),
            timestamp(start_ms)
        )
    });
    vec![Effect::Open(moment)]
}

/// `q`: quit; while a job runs, ask first.
fn quit_or_ask(m: &mut Model) {
    if m.progress.busy() {
        m.confirm = Some(Confirm::Quit);
    } else {
        m.should_quit = true;
    }
}

/// `A` (or with `dry_run`, `D`): every accepted fix of the view on screen. Not while a scan runs
/// (apply would contend with its reader for the disks, and findings are still coming) or apply
/// does, nor with nothing accepted. `A` asks first; a dry run writes nothing, so it starts at once.
fn apply_key(m: &mut Model, dry_run: bool) -> Vec<Effect> {
    if m.progress.running() {
        m.status = Some("the scan is still running".into());
        return Vec::new();
    }
    if m.progress.applying() {
        m.status = Some("already applying".into());
        return Vec::new();
    }
    if m.apply_counts().0 == 0 {
        m.status = Some("no accepted fixes".into());
        return Vec::new();
    }
    if dry_run {
        return start_apply(m, true);
    }
    m.confirm = Some(Confirm::Apply);
    Vec::new()
}

/// Start apply (or a dry run) on the view as it stands: decisions made meanwhile count from the
/// next. Running from here, so a `q` before its first step still asks.
fn start_apply(m: &mut Model, dry_run: bool) -> Vec<Effect> {
    // A view's name is its run's, or its paths quoted: `subcheck apply` takes either back.
    let mut resume = format!("subcheck apply {}", m.run.name);
    if dry_run {
        resume.push_str(" --dry-run");
    }
    if m.karaoke {
        resume.push_str(" --karaoke");
    }
    m.progress.start_apply(dry_run, resume);
    vec![Effect::StartApply {
        run: m.run.clone(),
        dry_run,
        karaoke: m.karaoke,
    }]
}

/// A step of the screen's apply: the files view follows it. A file done marks its findings (a fix
/// made, or found made, by the store's mark instead), takes the store's new `applied` marks, and
/// its record takes the size and mtime its checks moved to, so the view matches the store.
fn applied(m: &mut Model, event: apply::Event) -> Vec<Effect> {
    m.progress.apply_event(&event);
    if let apply::Event::Done {
        path,
        outcomes,
        marked,
        restamped,
        ..
    } = event
    {
        let dry_run = m.progress.apply.as_ref().is_some_and(|a| a.dry_run);
        for (id, outcome) in &outcomes {
            match Mark::of(outcome, dry_run) {
                Some(mark) => {
                    m.marks.insert(*id, mark);
                }
                None => {
                    m.marks.remove(id);
                }
            }
        }
        m.run.applied.extend(marked);
        // Every model record for the path takes the new stamp, even if only one check kind moved:
        // only `file_notes` reads it, and `v` re-reads the store.
        if let Some((size, mtime)) = restamped {
            for r in m.run.files.iter_mut().filter(|r| r.path == path) {
                r.size = size;
                r.mtime = mtime;
            }
        }
    }
    Vec::new()
}

/// A key in the files view: moving over the files, `⏎` to a file's findings, `Tab` back. The
/// decision and mpv keys belong to the findings, except `u`: the status line of a late join
/// says "u undoes" wherever the user is, so it undoes here too. `c` checks the selected file again.
/// `A` and `D` apply, as in the findings.
fn files_key(m: &mut Model, key: KeyCode, now: Timestamp) -> Vec<Effect> {
    let last = m.progress.files.len().saturating_sub(1);
    match key {
        KeyCode::Char('u') => return undo(m, now),
        KeyCode::Char('q') => quit_or_ask(m),
        KeyCode::Char('?') => m.help = true,
        KeyCode::Tab => m.pane = Pane::Findings,
        KeyCode::Char('v') => return switch_view(m),
        KeyCode::Char('A') => return apply_key(m, false),
        KeyCode::Char('D') => return apply_key(m, true),
        KeyCode::Down | KeyCode::Char('j') => m.file_selected = (m.file_selected + 1).min(last),
        KeyCode::Up | KeyCode::Char('k') => m.file_selected = m.file_selected.saturating_sub(1),
        KeyCode::PageDown => m.file_selected = (m.file_selected + PAGE).min(last),
        KeyCode::PageUp => m.file_selected = m.file_selected.saturating_sub(PAGE),
        KeyCode::Home | KeyCode::Char('g') => m.file_selected = 0,
        KeyCode::End | KeyCode::Char('G') => m.file_selected = last,
        KeyCode::Enter => file_findings(m),
        KeyCode::Char('c') => return recheck(m),
        _ => {}
    }
    Vec::new()
}

/// The other view `v` asked for has been read, or could not be: take it, or say why the screen is
/// as it was.
fn switched(
    m: &mut Model,
    result: Result<(Run, HashMap<String, FileNote>), String>,
) -> Vec<Effect> {
    match result {
        Ok((run, notes)) => m.switch(run, notes),
        Err(e) => m.status = Some(format!("could not switch views: {e}")),
    }
    Vec::new()
}

/// `v`: the other view of the screen's findings. From a run's view, the run's folders; from a paths
/// view, the newest run that named the selected file: the selected entry's (a row's first
/// member's), or in the files view, or with nothing selected in the list, the selected file's. Not
/// while a job runs: its progress belongs to the view it started in.
fn switch_view(m: &mut Model) -> Vec<Effect> {
    if m.progress.running() {
        m.status = Some("the scan is still running".into());
        return Vec::new();
    }
    if m.progress.applying() {
        m.status = Some("apply is still running".into());
        return Vec::new();
    }
    if !m.run.of_paths {
        return vec![Effect::Switch(Switch::FoldersOf(m.run.name.clone()))];
    }
    let in_list = match m.pane {
        Pane::Findings => m.selected_entry().map(|e| {
            let i = match e {
                Entry::Row(r) => m.rows[r].members[0],
                Entry::Member(_, i) => i,
            };
            m.run.findings[i].file.clone()
        }),
        Pane::Files => None,
    };
    in_list
        .or_else(|| {
            m.progress
                .files
                .get(m.file_selected)
                .map(|f| f.path.clone())
        })
        .map(|p| Effect::Switch(Switch::RunNaming(p)))
        .into_iter()
        .collect()
}

/// `c` on the selected file: find the run it is checked again into, then ask. A file the scan is
/// checking is being checked already, and one `c` queued is queued already.
fn recheck(m: &mut Model) -> Vec<Effect> {
    // A check would read a file apply may be remuxing or swapping.
    if m.progress.applying() {
        m.status = Some("apply is still running".into());
        return Vec::new();
    }
    let Some(file) = m.progress.files.get(m.file_selected) else {
        return Vec::new();
    };
    let path = file.path.clone();
    let queued_again = file.stage == Stage::QueuedAgain;
    let name = m.label(&path);
    if m.progress.checking(&path).is_some() {
        m.status = Some(format!("{name} is being checked now"));
        return Vec::new();
    }
    if queued_again {
        m.status = Some(format!(
            "{name} will be checked again when the scan is done"
        ));
        return Vec::new();
    }
    let run = (!m.run.of_paths).then(|| m.run.name.clone());
    vec![Effect::FindRun { path, run }]
}

/// `y` to checking `path` again into `run`: at once with no scan running, else once it ends.
fn again(m: &mut Model, path: String, run: String) -> Vec<Effect> {
    if !m.progress.running() {
        return vec![Effect::Recheck {
            run,
            paths: vec![path],
        }];
    }
    m.status = Some(format!(
        "{} will be checked again when the scan is done",
        m.label(&path)
    ));
    m.progress.queue_again(&path, &run);
    Vec::new()
}

/// The run `c` checks a file again into, and its LLM, for the question; or why there is none.
fn run_found(m: &mut Model, path: String, found: Result<(String, String), String>) -> Vec<Effect> {
    match found {
        Ok((run, backend)) => m.confirm = Some(Confirm::Recheck { path, run, backend }),
        Err(e) => m.status = Some(e),
    }
    Vec::new()
}

/// The files `c` queued into the first run among them, as one scan; the rest wait for it to end.
fn next_again(m: &mut Model) -> Vec<Effect> {
    m.progress
        .next_batch()
        .map(|(run, paths)| Effect::Recheck { run, paths })
        .into_iter()
        .collect()
}

/// A scan `c` started, or why none did: then the files asked for that are still on disk fail with
/// the reason (so `c` works on them again), and the next run's queued files go. A file left out is
/// no longer on disk.
fn started(
    m: &mut Model,
    job: Result<Job, String>,
    missing: &[String],
    paths: &[String],
) -> Vec<Effect> {
    let effects = match job {
        Ok(job) => {
            m.progress.start(&job.run, job.resume, true);
            Vec::new()
        }
        Err(e) => {
            for p in paths.iter().filter(|p| !missing.contains(p)) {
                m.progress.fail(p, &e);
            }
            m.status = Some(e);
            next_again(m)
        }
    };
    for p in missing {
        m.progress.fail(p, "no such file");
    }
    if let Some(p) = missing.first() {
        m.status = Some(format!("{}: no such file", m.label(p)));
    }
    settle_quit(m, &effects);
    effects
}

/// A question to stop the scan stays while a queued batch takes over: `effects` start it, and
/// effects run before the next key is read. With nothing left to run it goes, so the status line
/// shows how the scan ended, or why the last batch did not start, and the next `q` quits at once.
fn settle_quit(m: &mut Model, effects: &[Effect]) {
    if m.confirm == Some(Confirm::Quit) && !m.progress.busy() && effects.is_empty() {
        m.confirm = None;
    }
}

/// `⏎` on a file: the findings, on the first entry with one of the file's, if the filter shows
/// one.
fn file_findings(m: &mut Model) {
    let Some(path) = m
        .progress
        .files
        .get(m.file_selected)
        .map(|f| f.path.clone())
    else {
        return;
    };
    let first = m
        .entries()
        .into_iter()
        .position(|e| m.targets(e).iter().any(|&i| m.finding(i).file == path));
    match first {
        Some(at) => {
            m.selected = at;
            m.pane = Pane::Findings;
        }
        None => m.status = Some(format!("no findings of {} shown", m.label(&path))),
    }
}

/// A step of the scan: its progress; and of the records it appended, its LLM calls' cost, and a
/// failed attempt's record, which leaves the file's findings standing. A file it saved is read back
/// from the store, which brings its findings with the decisions carried to them.
fn scanned(m: &mut Model, event: scan::Event) -> Vec<Effect> {
    m.progress.scan_event(&event);
    let scan::Event::Appended(records) = event else {
        return Vec::new();
    };
    let mut effects = Vec::new();
    for r in records {
        match r {
            Record::File(f) if f.error.is_none() => effects.push(Effect::ReadBack(f.path)),
            r @ (Record::File(_) | Record::Usage(_)) => m.run.add(r),
            // They come with the read-back, as the store has them by then.
            Record::Finding(_) | Record::Decision(_) => {}
        }
    }
    effects
}

/// A file read back from the store: its findings swapped in ([`Model::refresh`]), and the new ones
/// taking their rows' decisions ([`inherit`]).
fn read_back(m: &mut Model, path: &str, view: Result<Run, String>, now: Timestamp) -> Vec<Effect> {
    match view {
        Ok(view) => {
            let new = m.refresh(path, view);
            inherit(m, &new, now)
        }
        Err(e) => {
            m.status = Some(format!("could not read {} back: {e}", m.label(path)));
            Vec::new()
        }
    }
}

/// The decision each of the `new` findings takes from its row: a typo finding whose row's other
/// members are all decided alike (the same state, the same wording or none) takes theirs. A
/// timing finding takes none: each episode's fix can differ. Nor does one that came with a
/// decision carried over from the file's earlier check of the same line and fix. One undo step for
/// them all.
fn inherit(m: &Model, new: &[usize], now: Timestamp) -> Vec<Effect> {
    let mut before = Vec::new();
    let mut decisions = Vec::new();
    for &i in new {
        let f = m.finding(i);
        if f.timing.is_some() || m.run.decisions.contains_key(&f.id) {
            continue;
        }
        let Some(row) = m.rows.iter().find(|r| r.members.contains(&i)) else {
            continue;
        };
        let mut others = row
            .members
            .iter()
            .filter(|j| !new.contains(j))
            .map(|&j| m.run.decisions.get(&m.finding(j).id));
        let Some(Some(first)) = others.next() else {
            continue;
        };
        let alike =
            others.all(|d| d.is_some_and(|d| d.state == first.state && d.edited == first.edited));
        if first.state == State::Pending || !alike {
            continue;
        }
        let id = m.finding(i).id;
        before.push((id, None));
        decisions.push(Decision {
            finding: id,
            state: first.state,
            edited: first.edited.clone(),
            at: now,
        });
    }
    if decisions.is_empty() {
        return Vec::new();
    }
    vec![Effect::Write {
        action: Action::Inherit { before },
        decisions,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::apply::patch::Outcome;
    use crate::review::model::Mark;
    use ratatui::layout::Rect;

    use crate::review::model::tests_support::{self, finding, path, sample};
    use crate::scan::Event;
    use crate::store::{Decided, Record};

    fn now() -> Timestamp {
        "2026-10-17T12:00:00Z".parse().unwrap()
    }

    fn key(m: &mut Model, code: KeyCode) -> Vec<Effect> {
        update(m, Msg::Key(code), now())
    }

    fn press(m: &mut Model, c: char) -> Vec<Effect> {
        key(m, KeyCode::Char(c))
    }

    /// Run a Write effect as if the store took it, and feed the result back.
    fn written(m: &mut Model, effects: Vec<Effect>) {
        let [Effect::Write { action, decisions }] = <[Effect; 1]>::try_from(effects).unwrap()
        else {
            panic!("expected one write");
        };
        let n = decisions.len();
        update(
            m,
            Msg::Written {
                action,
                decisions,
                result: Ok(vec![Decided::Written; n]),
            },
            now(),
        );
    }

    fn decided(effects: &[Effect]) -> Vec<(u64, State, Option<&str>)> {
        match effects {
            [Effect::Write { decisions, .. }] => decisions
                .iter()
                .map(|d| (d.finding, d.state, d.edited.as_deref()))
                .collect(),
            _ => panic!("expected one write: {effects:?}"),
        }
    }

    /// What `scan` appends for episode `ep`: its file record, then its findings.
    fn appended(ep: u32, findings: Vec<crate::store::Finding>) -> Msg {
        let mut records = vec![Record::File(tests_support::file(ep))];
        records.extend(findings.into_iter().map(Record::Finding));
        Msg::Scan(Event::Appended(records))
    }

    /// Episode `ep` read back once `scan` saved `findings` for it (see `tests_support::read_back`).
    fn saved(m: &Model, ep: u32, findings: Vec<crate::store::Finding>) -> Msg {
        Msg::ReadBack {
            path: path(ep),
            view: Ok(tests_support::read_back(m, ep, findings)),
        }
    }

    const TYPO: &str = "The beat gets trough to our mind";
    const FIXED: &str = "The beat gets through to our mind";

    fn decide_as(m: &mut Model, ids: &[u64], state: State, edited: Option<&str>) {
        for &id in ids {
            m.run.decisions.insert(
                id,
                Decision {
                    finding: id,
                    state,
                    edited: edited.map(Into::into),
                    at: now(),
                },
            );
        }
    }

    /// The sample run with a scan of E04 and E05 under way.
    fn scanning() -> Model {
        let mut m = sample();
        let queued = vec![PathBuf::from(path(4)), PathBuf::from(path(5))];
        update(&mut m, Msg::Scan(Event::Queued(queued)), now());
        m
    }

    #[test]
    fn a_typo_joining_a_row_decided_one_way_takes_that_decision_and_u_takes_it_back() {
        let mut m = sample();
        let mine = "The beat gets through to our minds";
        decide_as(&mut m, &[1, 2, 4], State::Accepted, Some(mine));
        let msg = saved(&m, 4, vec![finding(5, 4, TYPO, FIXED, 165_060)]);
        let effects = update(&mut m, msg, now());
        assert_eq!(decided(&effects), [(5, State::Accepted, Some(mine))]);
        written(&mut m, effects);
        assert_eq!(m.run.state_of(5), State::Accepted);
        assert_eq!(
            m.status.as_deref(),
            Some("E04: a new finding took its row's decision: accepted (u undoes)")
        );
        let effects = press(&mut m, 'u');
        assert_eq!(decided(&effects), [(5, State::Pending, None)]);
    }

    #[test]
    fn a_typo_that_comes_with_a_decision_carried_over_keeps_it_over_its_rows() {
        // E04 checked again: its lyric comes back rejected, carried over from its earlier check,
        // into a row the other episodes accepted.
        let mut m = sample();
        decide_as(&mut m, &[1, 2, 4], State::Accepted, None);
        let carried = Decision {
            finding: 5,
            state: State::Rejected,
            edited: None,
            at: now(),
        };
        let mut view = tests_support::read_back(&m, 4, vec![finding(5, 4, TYPO, FIXED, 165_060)]);
        view.decisions.insert(5, carried);
        let effects = update(
            &mut m,
            Msg::ReadBack {
                path: path(4),
                view: Ok(view),
            },
            now(),
        );
        assert_eq!(effects, []);
        assert_eq!(m.run.state_of(5), State::Rejected);
    }

    #[test]
    fn a_late_join_that_takes_its_rows_decision_leaves_the_selection_on_its_row() {
        // A third row, E03's, below the snacks row: one for the selection to slip onto.
        let mut m = sample();
        let view = tests_support::read_back(
            &m,
            3,
            vec![finding(5, 3, "An erly line.", "An early line.", 2_000_000)],
        );
        m.refresh(&path(3), view);
        decide_as(&mut m, &[1, 2, 4], State::Accepted, None);
        // Pending only: the lyric's row is hidden until E04's lyric shows it for a moment.
        m.filter = Filter::Only(State::Pending);
        m.selected = 0;
        assert_eq!(m.selected_entry(), Some(Entry::Row(1)));
        let msg = saved(&m, 4, vec![finding(6, 4, TYPO, FIXED, 165_060)]);
        let effects = update(&mut m, msg, now());
        written(&mut m, effects);
        assert_eq!(m.entries(), [Entry::Row(1), Entry::Row(2)]);
        assert_eq!(m.selected_entry(), Some(Entry::Row(1)), "the snacks row");
        // Accepted only, the lyric's row expanded and the snacks row selected: the decision shows
        // the new member above it.
        let mut m = sample();
        decide_as(&mut m, &[1, 2, 3, 4], State::Accepted, None);
        m.filter = Filter::Only(State::Accepted);
        m.expanded.insert(0);
        m.selected = 4;
        assert_eq!(m.selected_entry(), Some(Entry::Row(1)));
        let msg = saved(&m, 4, vec![finding(5, 4, TYPO, FIXED, 165_060)]);
        let effects = update(&mut m, msg, now());
        written(&mut m, effects);
        assert_eq!(m.entries().len(), 6);
        assert_eq!(m.selected_entry(), Some(Entry::Row(1)), "the snacks row");
    }

    #[test]
    fn a_late_join_that_hides_a_row_above_the_selection_keeps_its_row_on_the_same_screen_line() {
        // Ten more rows, pending: the list is taller than the five rows the screen shows.
        let mut m = sample();
        let more: Vec<crate::store::Finding> = (0..10_u64)
            .map(|n| {
                let line = format!("Line {n} has an erly typo.");
                finding(
                    10 + n,
                    3,
                    &line,
                    "Fixed.",
                    3_000_000 + 10_000 * i64::try_from(n).unwrap(),
                )
            })
            .collect();
        let view = tests_support::read_back(&m, 3, more);
        m.refresh(&path(3), view);
        decide_as(&mut m, &[1, 2, 4], State::Accepted, None);
        m.filter = Filter::Only(State::Pending);
        m.selected = 8;
        m.set_area(Rect::new(0, 0, 100, 8));
        let line = |m: &Model| m.selected - m.list_scroll(m.area);
        let selected = m.selected_entry();
        assert_eq!(line(&m), 4, "scrolled to the bottom of the screen");
        // E04's lyric joins the decided lyric row: the row shows above the selection for a
        // moment, then the inherited decision hides it again.
        let msg = saved(&m, 4, vec![finding(30, 4, TYPO, FIXED, 165_060)]);
        let effects = update(&mut m, msg, now());
        written(&mut m, effects);
        assert_eq!(m.selected_entry(), selected);
        assert_eq!(m.selected, 8);
        assert_eq!(line(&m), 4, "the selected row did not move on screen");
    }

    #[test]
    fn u_in_the_files_view_takes_a_late_join_back_and_names_its_episode() {
        let mut m = sample();
        decide_as(&mut m, &[1, 2, 4], State::Accepted, None);
        let msg = saved(&m, 4, vec![finding(5, 4, TYPO, FIXED, 165_060)]);
        let effects = update(&mut m, msg, now());
        written(&mut m, effects);
        m.pane = Pane::Files;
        let effects = press(&mut m, 'u');
        assert_eq!(decided(&effects), [(5, State::Pending, None)]);
        written(&mut m, effects);
        assert_eq!(m.run.state_of(5), State::Pending);
        assert_eq!(m.status.as_deref(), Some("undone: 1 finding · E04"));
        assert_eq!(m.pane, Pane::Files);
    }

    #[test]
    fn an_undo_across_two_episodes_names_both_in_order_of_first_appearance() {
        let mut m = sample();
        decide_as(&mut m, &[1, 2, 4], State::Accepted, None);
        let msg = saved(&m, 4, vec![finding(5, 4, TYPO, FIXED, 165_060)]);
        let effects = update(&mut m, msg, now());
        written(&mut m, effects);
        let msg = saved(&m, 5, vec![finding(6, 5, TYPO, FIXED, 165_060)]);
        let effects = update(&mut m, msg, now());
        written(&mut m, effects);
        // One step taking back findings of E05 and E04 at once.
        m.undo.push(vec![(6, None), (5, None)]);
        let effects = press(&mut m, 'u');
        written(&mut m, effects);
        assert_eq!(m.status.as_deref(), Some("undone: 2 findings · E05, E04"));
    }

    #[test]
    fn a_new_finding_stays_pending_in_a_mixed_row_and_in_a_timing_row() {
        let mut m = sample();
        decide_as(&mut m, &[1], State::Accepted, None);
        decide_as(&mut m, &[2], State::Rejected, None);
        let msg = saved(&m, 4, vec![finding(5, 4, TYPO, FIXED, 165_060)]);
        let effects = update(&mut m, msg, now());
        assert_eq!(effects, []);
        assert_eq!(m.run.state_of(5), State::Pending);
        // A season's track accepted at −1 frame: E04's fix may differ, so it waits.
        let mut m = tests_support::with_timing();
        decide_as(&mut m, &[11, 12, 13], State::Accepted, None);
        let msg = saved(&m, 4, vec![tests_support::timing(14, 4, false)]);
        let effects = update(&mut m, msg, now());
        assert_eq!(effects, []);
        assert_eq!(m.run.state_of(14), State::Pending);
    }

    #[test]
    fn a_new_finding_stays_pending_in_a_row_accepted_in_two_wordings_or_with_one_pending() {
        let join = |m: &mut Model| {
            let msg = saved(m, 4, vec![finding(5, 4, TYPO, FIXED, 165_060)]);
            update(m, msg, now())
        };
        // Every member accepted, E03's in words of the user's own: no one wording to take.
        let mut m = sample();
        decide_as(&mut m, &[1, 2], State::Accepted, None);
        let mine = "The beat gets through to our minds";
        decide_as(&mut m, &[4], State::Accepted, Some(mine));
        assert_eq!(join(&mut m), []);
        assert_eq!(m.run.state_of(5), State::Pending);
        // E01 and E02 accepted, E03 not yet decided.
        let mut m = sample();
        decide_as(&mut m, &[1, 2], State::Accepted, None);
        assert_eq!(join(&mut m), []);
        assert_eq!(m.run.state_of(5), State::Pending);
    }

    #[test]
    fn quitting_while_the_scan_runs_asks_first() {
        let status = |m: &Model| m.status_text(Rect::new(0, 0, 160, 1));
        let mut m = scanning();
        press(&mut m, 'q');
        assert!(!m.should_quit);
        assert_eq!(
            status(&m).as_deref(),
            Some("scan is on 0/2 — stop it and quit? y/n")
        );
        press(&mut m, 'n');
        assert!(!m.should_quit && m.confirm.is_none());
        press(&mut m, 'q');
        press(&mut m, 'y');
        assert!(m.should_quit && m.progress.running());
        // Ctrl-C asks too, and a second one quits.
        let mut m = scanning();
        update(&mut m, Msg::Quit, now());
        assert!(!m.should_quit);
        update(&mut m, Msg::Quit, now());
        assert!(m.should_quit);
        // Once the scan has ended, q quits at once.
        let mut m = scanning();
        update(
            &mut m,
            Msg::ScanEnded(Ok(crate::scan::Summary::default())),
            now(),
        );
        press(&mut m, 'q');
        assert!(m.should_quit);
    }

    #[test]
    fn a_quit_question_goes_when_the_scan_ends() {
        let status = |m: &Model| m.status_text(Rect::new(0, 0, 160, 1));
        let mut m = scanning();
        press(&mut m, 'q');
        assert!(m.confirm.is_some());
        let summary = crate::scan::Summary {
            run: "r".into(),
            files: 2,
            checked: 2,
            ..crate::scan::Summary::default()
        };
        update(&mut m, Msg::ScanEnded(Ok(summary)), now());
        // Nothing left to stop: the line gives the summary, and `q` quits at once.
        assert_eq!(m.confirm, None);
        assert_eq!(
            status(&m).as_deref(),
            Some(
                "run r: 2 files, 2 checked, 0 failed, 0 unchanged since an earlier scan; 0 findings"
            )
        );
        press(&mut m, 'q');
        assert!(m.should_quit);
    }

    #[test]
    fn a_quit_question_stays_while_a_queued_batch_takes_over_and_goes_after_the_last() {
        let status = |m: &Model| m.status_text(Rect::new(0, 0, 160, 1));
        let mut m = scanning();
        for (ep, run) in [(1, "r"), (2, "s")] {
            m.confirm = Some(Confirm::Recheck {
                path: path(ep),
                run: run.into(),
                backend: "claude".into(),
            });
            assert_eq!(press(&mut m, 'y'), []);
        }
        press(&mut m, 'q');
        let ended = update(&mut m, Msg::ScanEnded(Ok(scan::Summary::default())), now());
        assert_eq!(
            ended,
            [Effect::Recheck {
                run: "r".into(),
                paths: vec![path(1)],
            }]
        );
        assert_eq!(m.confirm, Some(Confirm::Quit));
        let job = Job {
            run: "r".into(),
            resume: "subcheck scan --run r --force /m/E01.mkv".into(),
        };
        update(
            &mut m,
            Msg::Started {
                job: Ok(job),
                missing: vec![],
                paths: vec![path(1)],
            },
            now(),
        );
        update(
            &mut m,
            Msg::Scan(Event::Queued(vec![PathBuf::from(path(1))])),
            now(),
        );
        // The question now asks about the batch that took over.
        assert_eq!(
            status(&m).as_deref(),
            Some("scan is on 0/1 · 1 queued to check again — stop it and quit? y/n")
        );
        // r's batch ends and s's cannot start: nothing is left to stop, so the question goes.
        update(&mut m, Msg::ScanEnded(Ok(scan::Summary::default())), now());
        assert_eq!(m.confirm, Some(Confirm::Quit));
        update(
            &mut m,
            Msg::Started {
                job: Err("run s: no LLM is called gpt".into()),
                missing: vec![],
                paths: vec![path(2)],
            },
            now(),
        );
        assert_eq!(m.confirm, None);
        // s's files failed with the reason: the outro does not list them as left unchecked.
        assert!(
            !m.progress
                .outro()
                .iter()
                .any(|l| l.starts_with("Not checked again"))
        );
        press(&mut m, 'q');
        assert!(m.should_quit);
    }

    #[test]
    fn a_quit_question_stays_when_a_queued_batch_cannot_start_and_another_goes() {
        let mut m = scanning();
        for (ep, run) in [(1, "r"), (2, "s")] {
            m.confirm = Some(Confirm::Recheck {
                path: path(ep),
                run: run.into(),
                backend: "claude".into(),
            });
            assert_eq!(press(&mut m, 'y'), []);
        }
        press(&mut m, 'q');
        update(&mut m, Msg::ScanEnded(Ok(scan::Summary::default())), now());
        let effects = update(
            &mut m,
            Msg::Started {
                job: Err("run r: no LLM is called gpt".into()),
                missing: vec![],
                paths: vec![path(1)],
            },
            now(),
        );
        assert_eq!(
            effects,
            [Effect::Recheck {
                run: "s".into(),
                paths: vec![path(2)],
            }]
        );
        assert_eq!(m.confirm, Some(Confirm::Quit));
        let job = Job {
            run: "s".into(),
            resume: "subcheck scan --run s --force /m/E02.mkv".into(),
        };
        update(
            &mut m,
            Msg::Started {
                job: Ok(job),
                missing: vec![],
                paths: vec![path(2)],
            },
            now(),
        );
        // `y` over it quits, stopping the batch that took over.
        press(&mut m, 'y');
        assert!(m.should_quit && m.progress.running());
    }

    #[test]
    fn the_status_line_follows_the_scan_then_gives_its_summary() {
        let status = |m: &Model| m.status_text(Rect::new(0, 0, 160, 1));
        let mut m = scanning();
        assert_eq!(status(&m).as_deref(), Some("scan 0/2 · 0 findings"));
        let failed = Event::Finished {
            path: path(4).into(),
            error: Some("LLM: boom".into()),
            findings: 0,
            lines: vec![],
        };
        update(&mut m, Msg::Scan(failed), now());
        assert_eq!(
            status(&m).as_deref(),
            Some("scan 1/2 · 0 findings · 1 failed")
        );
        // A message of its own takes the line until the next key.
        m.status = Some("opened in mpv".into());
        assert_eq!(status(&m).as_deref(), Some("opened in mpv"));
        press(&mut m, 'j');
        let summary = crate::scan::Summary {
            run: "r".into(),
            files: 2,
            checked: 1,
            failed: 1,
            ..crate::scan::Summary::default()
        };
        update(&mut m, Msg::ScanEnded(Ok(summary)), now());
        assert_eq!(
            status(&m).as_deref(),
            Some(
                "run r: 2 files, 1 checked, 1 failed, 0 unchanged since an earlier scan; 0 findings"
            )
        );
        update(&mut m, Msg::Tick, now());
        assert_eq!(m.progress.ticks, 1);
    }

    #[test]
    fn tab_shows_the_files_and_enter_goes_to_a_files_first_finding() {
        let mut m = sample();
        m.selected = 1;
        key(&mut m, KeyCode::Tab);
        assert_eq!(m.pane, Pane::Files);
        press(&mut m, 'j');
        assert_eq!(m.file_selected, 1);
        // The decision and mpv keys belong to the findings.
        assert_eq!(press(&mut m, 'a'), []);
        assert_eq!(press(&mut m, 'o'), []);
        // E02's first entry is the lyric's row, which has its finding.
        key(&mut m, KeyCode::Enter);
        assert_eq!((m.pane, m.selected), (Pane::Findings, 0));
        key(&mut m, KeyCode::Tab);
        key(&mut m, KeyCode::Tab);
        assert_eq!(m.pane, Pane::Findings);
    }

    #[test]
    fn a_fatal_message_ends_the_screen_with_it() {
        let mut m = scanning();
        update(&mut m, Msg::Fatal("the scan panicked: boom".into()), now());
        assert!(m.should_quit);
        assert_eq!(m.fatal.as_deref(), Some("the scan panicked: boom"));
    }

    fn states(m: &Model) -> Vec<State> {
        m.run
            .findings
            .iter()
            .map(|f| m.run.state_of(f.id))
            .collect()
    }

    #[test]
    fn a_decision_on_a_group_covers_every_member_and_moves_on() {
        let mut m = sample();
        let effects = press(&mut m, 'a');
        assert_eq!(
            decided(&effects),
            [
                (1, State::Accepted, None),
                (2, State::Accepted, None),
                (4, State::Accepted, None)
            ]
        );
        written(&mut m, effects);
        assert_eq!(
            states(&m),
            [
                State::Accepted,
                State::Accepted,
                State::Pending,
                State::Accepted
            ]
        );
        assert_eq!(m.status.as_deref(), Some("accepted 3 findings"));
        // The decided row stays in the list; the selection moved on to the next.
        assert_eq!(m.entries(), [Entry::Row(0), Entry::Row(1)]);
        assert_eq!(m.selected_entry(), Some(Entry::Row(1)));
    }

    #[test]
    fn under_all_the_selection_moves_past_the_decided_row() {
        let mut m = sample();
        m.filter = Filter::All;
        let effects = press(&mut m, 'r');
        written(&mut m, effects);
        assert_eq!(m.selected_entry(), Some(Entry::Row(1)));
        // Nothing left to change: no write.
        m.selected = 0;
        assert_eq!(press(&mut m, 'r'), []);
        assert_eq!(m.status.as_deref(), Some("already rejected"));
    }

    #[test]
    fn after_a_decision_the_selection_skips_rows_already_decided() {
        let mut m = crate::review::model::tests_support::many(4);
        m.selected = 1;
        let effects = press(&mut m, 'r');
        written(&mut m, effects);
        assert_eq!(m.selected, 2, "E03, the next row");
        m.selected = 0;
        let effects = press(&mut m, 'a');
        written(&mut m, effects);
        assert_eq!(m.selected, 2, "past E02, decided");
        // Nothing pending below the last row: the selection stays on it.
        m.selected = 3;
        let effects = press(&mut m, 'a');
        written(&mut m, effects);
        assert_eq!(m.selected, 3);
    }

    #[test]
    fn under_a_decided_filter_a_decision_moves_to_the_row_that_takes_its_place() {
        let mut m = crate::review::model::tests_support::many(4);
        for id in 1..=4 {
            m.run.decisions.insert(
                id,
                Decision {
                    finding: id,
                    state: State::Rejected,
                    edited: None,
                    at: now(),
                },
            );
        }
        m.filter = Filter::Only(State::Rejected);
        m.selected = 1;
        let effects = press(&mut m, 'a');
        written(&mut m, effects);
        // Nothing here is pending: E02 left, and E03 took its place.
        assert_eq!(m.selected_entry(), Some(Entry::Row(2)));
    }

    #[test]
    fn deciding_a_group_member_by_member_lands_on_the_next_row() {
        // A third row, so a selection that runs one row too far shows.
        let mut run = crate::review::model::tests_support::run();
        run.findings
            .push(crate::review::model::tests_support::finding(
                5,
                3,
                "A later line.",
                "A later line!",
                1_000_000,
            ));
        let mut m = Model::new(run, std::collections::HashMap::new());
        assert_eq!(m.rows.len(), 3);
        key(&mut m, KeyCode::Enter);
        m.selected = 1;
        for _ in 0..3 {
            let effects = press(&mut m, 'a');
            written(&mut m, effects);
        }
        // The last member took its row with it; the row after the group is next, not skipped.
        assert_eq!(m.selected_entry(), Some(Entry::Row(1)));
    }

    #[test]
    fn a_timing_fix_is_accepted_or_rejected_not_edited() {
        let mut m = crate::review::model::tests_support::with_timing();
        m.selected = 1;
        assert_eq!(press(&mut m, 'e'), []);
        assert_eq!(m.editor, None);
        assert_eq!(
            m.status.as_deref(),
            Some("a timing fix is accepted or rejected, not edited")
        );
        // Accepting the row accepts each member's own fix.
        assert_eq!(
            decided(&press(&mut m, 'a')),
            [
                (11, State::Accepted, None),
                (12, State::Accepted, None),
                (13, State::Accepted, None)
            ]
        );
    }

    #[test]
    fn accepting_over_a_rewording_says_the_wording_went() {
        let mut m = sample();
        m.selected = 1;
        press(&mut m, 'e');
        update(&mut m, Msg::KillLine, now());
        for c in "Snacks every day!".chars() {
            press(&mut m, c);
        }
        let effects = key(&mut m, KeyCode::Enter);
        written(&mut m, effects);
        assert_eq!(m.status.as_deref(), Some("accepted 1 finding"));
        m.filter = Filter::All;
        m.selected = 1;
        let effects = press(&mut m, 'a');
        assert_eq!(decided(&effects), [(3, State::Accepted, None)]);
        written(&mut m, effects);
        assert_eq!(
            m.status.as_deref(),
            Some("accepted 1 finding, replacing your wording on 1 (u undoes)")
        );
    }

    #[test]
    fn undo_puts_back_every_member_and_selects_the_row() {
        let mut m = sample();
        let effects = press(&mut m, 'a');
        written(&mut m, effects);
        let effects = press(&mut m, 'u');
        assert_eq!(
            decided(&effects),
            [
                (1, State::Pending, None),
                (2, State::Pending, None),
                (4, State::Pending, None)
            ]
        );
        written(&mut m, effects);
        assert_eq!(states(&m), [State::Pending; 4]);
        assert_eq!(m.selected_entry(), Some(Entry::Row(0)));
        assert_eq!(
            m.status.as_deref(),
            Some("undone: 3 findings · E01, E02, E03")
        );
        assert_eq!(press(&mut m, 'u'), []);
        assert_eq!(m.status.as_deref(), Some("nothing to undo"));
    }

    #[test]
    fn x_clears_an_entry_back_to_pending_and_u_puts_it_back() {
        let mut m = sample();
        let effects = press(&mut m, 'a');
        written(&mut m, effects);
        m.selected = 0;
        let effects = press(&mut m, 'x');
        assert_eq!(
            decided(&effects),
            [
                (1, State::Pending, None),
                (2, State::Pending, None),
                (4, State::Pending, None)
            ]
        );
        written(&mut m, effects);
        assert_eq!(states(&m), [State::Pending; 4]);
        assert_eq!(m.status.as_deref(), Some("cleared 3 findings"));
        assert_eq!(
            m.selected_entry(),
            Some(Entry::Row(1)),
            "on to the next pending row, as after a and r"
        );
        assert_eq!(press(&mut m, 'x'), []);
        assert_eq!(m.status.as_deref(), Some("nothing to clear"));
        let effects = press(&mut m, 'u');
        assert_eq!(
            decided(&effects),
            [
                (1, State::Accepted, None),
                (2, State::Accepted, None),
                (4, State::Accepted, None)
            ]
        );
    }

    #[test]
    fn x_on_a_member_clears_it_alone_and_on_a_row_only_the_decided() {
        let mut m = sample();
        let effects = press(&mut m, 'a');
        written(&mut m, effects);
        m.selected = 0;
        key(&mut m, KeyCode::Enter);
        m.selected = 2;
        assert_eq!(m.selected_entry(), Some(Entry::Member(0, 1)));
        let effects = press(&mut m, 'x');
        assert_eq!(decided(&effects), [(2, State::Pending, None)]);
        written(&mut m, effects);
        m.selected = 0;
        assert_eq!(
            decided(&press(&mut m, 'x')),
            [(1, State::Pending, None), (4, State::Pending, None)]
        );
    }

    #[test]
    fn undo_restores_an_earlier_wording() {
        let mut m = sample();
        m.selected = 1;
        press(&mut m, 'e');
        update(&mut m, Msg::KillLine, now());
        for c in "Snacks every day!".chars() {
            press(&mut m, c);
        }
        let effects = key(&mut m, KeyCode::Enter);
        written(&mut m, effects);
        m.filter = Filter::All;
        m.selected = 1;
        let effects = press(&mut m, 'r');
        written(&mut m, effects);
        let effects = press(&mut m, 'u');
        assert_eq!(
            decided(&effects),
            [(3, State::Accepted, Some("Snacks every day!"))]
        );
    }

    #[test]
    fn a_member_is_decided_alone_and_the_row_decides_the_rest() {
        let mut m = sample();
        m.filter = Filter::Only(State::Pending);
        key(&mut m, KeyCode::Enter);
        assert_eq!(
            m.entries(),
            [
                Entry::Row(0),
                Entry::Member(0, 0),
                Entry::Member(0, 1),
                Entry::Member(0, 3),
                Entry::Row(1)
            ]
        );
        m.selected = 2;
        let effects = press(&mut m, 'r');
        assert_eq!(decided(&effects), [(2, State::Rejected, None)]);
        written(&mut m, effects);
        // E02's member left the pending list; E03's took its place.
        assert_eq!(m.selected_entry(), Some(Entry::Member(0, 3)));
        assert_eq!(m.row_state(0), None, "mixed");
        m.selected = 0;
        let effects = press(&mut m, 'a');
        assert_eq!(
            decided(&effects),
            [(1, State::Accepted, None), (4, State::Accepted, None)],
            "only the members the filter shows"
        );
    }

    #[test]
    fn a_row_decision_keeps_a_member_decided_alone() {
        let mut m = sample();
        m.filter = Filter::All;
        key(&mut m, KeyCode::Enter);
        m.selected = 2;
        let effects = press(&mut m, 'r');
        written(&mut m, effects);
        // The row shows E02's rejection too, but its key decides only the pending members.
        m.selected = 0;
        let effects = press(&mut m, 'a');
        assert_eq!(
            decided(&effects),
            [(1, State::Accepted, None), (4, State::Accepted, None)]
        );
        written(&mut m, effects);
        assert_eq!(
            m.status.as_deref(),
            Some("accepted 2 findings, 1 kept as rejected")
        );
        // None pending: the row flips as a whole.
        m.selected = 0;
        let effects = press(&mut m, 'r');
        assert_eq!(
            decided(&effects),
            [(1, State::Rejected, None), (4, State::Rejected, None)]
        );
        written(&mut m, effects);
        assert_eq!(m.status.as_deref(), Some("rejected 2 findings"));
    }

    #[test]
    fn editing_a_row_rewords_its_pending_members() {
        let mut run = crate::review::model::tests_support::run();
        run.findings[1].original = "Let's have snacks together everyday.".into();
        run.findings[1].suggestion = "Let's have snacks together every day.".into();
        run.findings[1].occurrences[0].start_ms = 894_190;
        let mut m = Model::new(run, std::collections::HashMap::new());
        m.filter = Filter::All;
        // E02's two "everyday" findings are one row; reject the first alone.
        let row = m.rows.iter().position(|r| r.members == [1, 2]).unwrap();
        m.expanded.insert(row);
        m.selected = m
            .entries()
            .iter()
            .position(|&e| e == Entry::Member(row, 1))
            .unwrap();
        let effects = press(&mut m, 'r');
        written(&mut m, effects);
        m.selected = m
            .entries()
            .iter()
            .position(|&e| e == Entry::Row(row))
            .unwrap();
        press(&mut m, 'e');
        update(&mut m, Msg::KillLine, now());
        for c in "Snacks every day!".chars() {
            press(&mut m, c);
        }
        assert_eq!(
            decided(&key(&mut m, KeyCode::Enter)),
            [(3, State::Accepted, Some("Snacks every day!"))]
        );
    }

    #[test]
    fn editing_accepts_with_the_new_wording() {
        let mut m = sample();
        m.selected = 1;
        press(&mut m, 'e');
        assert_eq!(
            m.editor.as_ref().map(|e| e.text.as_str()),
            Some("Let's have snacks together every day.")
        );
        // Typed keys are text, `q` included.
        key(&mut m, KeyCode::Home);
        key(&mut m, KeyCode::Delete);
        press(&mut m, 'q');
        assert!(!m.should_quit);
        key(&mut m, KeyCode::Backspace);
        press(&mut m, 'l');
        let effects = key(&mut m, KeyCode::Enter);
        assert_eq!(
            decided(&effects),
            [(
                3,
                State::Accepted,
                Some("let's have snacks together every day.")
            )]
        );
        assert!(m.editor.is_none());
    }

    #[test]
    fn an_edit_equal_to_the_suggestion_is_a_plain_accept() {
        let mut m = sample();
        m.selected = 1;
        press(&mut m, 'e');
        let effects = key(&mut m, KeyCode::Enter);
        assert_eq!(decided(&effects), [(3, State::Accepted, None)]);
    }

    #[test]
    fn an_empty_edit_is_refused_and_esc_cancels() {
        let mut m = sample();
        press(&mut m, 'e');
        update(&mut m, Msg::KillLine, now());
        assert_eq!(key(&mut m, KeyCode::Enter), []);
        assert!(m.editor.is_some());
        assert_eq!(
            m.status.as_deref(),
            Some("the wording cannot be empty (Esc cancels)")
        );
        key(&mut m, KeyCode::Esc);
        assert!(m.editor.is_none());
        assert_eq!(states(&m), [State::Pending; 4]);
    }

    #[test]
    fn a_failed_write_changes_nothing() {
        let mut m = sample();
        let Some(Effect::Write { action, decisions }) = press(&mut m, 'a').pop() else {
            panic!("expected a write");
        };
        update(
            &mut m,
            Msg::Written {
                action,
                decisions,
                result: Err("disk full".into()),
            },
            now(),
        );
        assert_eq!(states(&m), [State::Pending; 4]);
        assert_eq!(m.undo.len(), 0);
        assert_eq!(m.status.as_deref(), Some("not saved: disk full"));
    }

    #[test]
    fn the_filter_cycles_and_keeps_the_selected_row() {
        let mut m = sample();
        assert_eq!(m.filter, Filter::All);
        m.selected = 1;
        press(&mut m, 'f');
        assert_eq!(m.filter, Filter::Only(State::Pending));
        assert_eq!(m.selected_entry(), Some(Entry::Row(1)));
        press(&mut m, 'f');
        assert_eq!(m.filter, Filter::Only(State::Accepted));
        assert_eq!(m.entries(), []);
        press(&mut m, 'f');
        press(&mut m, 'f');
        assert_eq!(m.filter, Filter::All);
    }

    #[test]
    fn review_opens_on_every_finding_at_the_first_pending_row() {
        let accepted = |id| Decision {
            finding: id,
            state: State::Accepted,
            edited: None,
            at: now(),
        };
        let opened = |ids: &[u64]| {
            let mut run = crate::review::model::tests_support::run();
            for &id in ids {
                run.decisions.insert(id, accepted(id));
            }
            Model::new(run, std::collections::HashMap::new())
        };
        let m = opened(&[1, 2, 4]);
        assert_eq!(m.entries(), [Entry::Row(0), Entry::Row(1)]);
        assert_eq!(m.selected_entry(), Some(Entry::Row(1)));
        // Nothing pending: the top.
        assert_eq!(opened(&[1, 2, 3, 4]).selected, 0);
    }

    #[test]
    fn n_and_p_jump_between_files() {
        let mut m = sample();
        press(&mut m, 'n');
        assert_eq!(m.selected_entry(), Some(Entry::Row(1)));
        press(&mut m, 'n');
        assert_eq!(m.status.as_deref(), Some("last file"));
        press(&mut m, 'p');
        assert_eq!(m.selected_entry(), Some(Entry::Row(0)));
        press(&mut m, 'p');
        assert_eq!(m.status.as_deref(), Some("first file"));
        // Expanded, the group's members are in their own files.
        key(&mut m, KeyCode::Enter);
        press(&mut m, 'n');
        assert_eq!(m.selected_entry(), Some(Entry::Member(0, 1)));
        key(&mut m, KeyCode::End);
        press(&mut m, 'p');
        assert_eq!(
            m.selected_entry(),
            Some(Entry::Member(0, 3)),
            "E02's row is after E03's member"
        );
    }

    #[test]
    fn enter_on_a_member_collapses_its_group() {
        let mut m = sample();
        key(&mut m, KeyCode::Enter);
        key(&mut m, KeyCode::Down);
        key(&mut m, KeyCode::Enter);
        assert_eq!(m.entries(), [Entry::Row(0), Entry::Row(1)]);
        assert_eq!(m.selected_entry(), Some(Entry::Row(0)));
    }

    #[test]
    fn o_opens_two_seconds_early_on_the_default_track() {
        let mut m = sample();
        assert_eq!(
            press(&mut m, 'o'),
            [Effect::Open(Moment {
                file: path(1),
                start_ms: 163_060,
                sid: Some(1),
                paused: false,
            })]
        );
        assert_eq!(m.status.as_deref(), Some("opening E01 at 0:02:43.06"));
        update(&mut m, Msg::Opened(Err("mpv not found".into())), now());
        assert_eq!(m.status.as_deref(), Some("mpv not found"));
    }

    #[test]
    fn o_opens_a_timing_finding_paused_on_the_frame_that_shows_it() {
        let mut m = crate::review::model::tests_support::with_timing();
        m.selected = 1;
        assert_eq!(
            press(&mut m, 'o'),
            [Effect::Open(Moment {
                file: path(1),
                start_ms: 763_763,
                sid: Some(1),
                paused: true,
            })]
        );
        assert_eq!(
            m.status.as_deref(),
            Some("opening E01 at 0:12:43.763, paused")
        );
    }

    #[test]
    fn a_timing_finding_with_no_line_of_its_track_to_open_says_so() {
        let mut m = crate::review::model::tests_support::with_timing();
        for f in &mut m.run.findings {
            if let Some(t) = &mut f.timing {
                for s in &mut t.stretches {
                    (s.first, s.last, s.frame_ms) = (None, None, None);
                }
            }
        }
        m.selected = 1;
        assert_eq!(press(&mut m, 'o'), []);
        assert_eq!(
            m.status.as_deref(),
            Some("nothing to open: no line of this track moves")
        );
    }

    #[test]
    fn a_missing_file_is_not_opened() {
        let mut m = sample();
        m.notes.insert(path(1), FileNote::Missing);
        assert_eq!(press(&mut m, 'o'), []);
        assert_eq!(
            m.status.as_deref(),
            Some(format!("file missing: {}", path(1)).as_str())
        );
    }

    #[test]
    fn playback_keys_ask_mpv_and_show_where_it_is() {
        let mut m = sample();
        let playback = |p| vec![Effect::Playback(p)];
        assert_eq!(press(&mut m, ' '), playback(Playback::Pause));
        assert_eq!(key(&mut m, KeyCode::Left), playback(Playback::Seek(-5)));
        assert_eq!(key(&mut m, KeyCode::Right), playback(Playback::Seek(5)));
        assert_eq!(press(&mut m, ','), playback(Playback::Seek(-1)));
        assert_eq!(press(&mut m, '.'), playback(Playback::Seek(1)));
        let at = |paused| {
            Msg::Played(Ok(PlayState {
                paused,
                at_ms: 171_060,
            }))
        };
        update(&mut m, at(true), now());
        assert_eq!(m.status.as_deref(), Some("⏸ 0:02:51.060"));
        update(&mut m, at(false), now());
        assert_eq!(m.status.as_deref(), Some("▶ 0:02:51.060"));
        update(
            &mut m,
            Msg::Played(Err("mpv is not open (o opens it)".into())),
            now(),
        );
        assert_eq!(m.status.as_deref(), Some("mpv is not open (o opens it)"));
    }

    #[test]
    fn in_the_editor_playback_keys_edit() {
        let mut m = sample();
        m.selected = 1;
        press(&mut m, 'e');
        assert_eq!(key(&mut m, KeyCode::Left), []);
        assert_eq!(press(&mut m, ' '), []);
        assert_eq!(press(&mut m, '.'), []);
        assert_eq!(
            m.editor.as_ref().map(|e| e.text.as_str()),
            Some("Let's have snacks together every day ..")
        );
    }

    /// `sample()` on a 110×20 screen: the list's rows are y 1 to 17 from x 1, the detail pane is
    /// x 55 on, and the status line y 19, with `? help` from x 104.
    fn on_screen() -> Model {
        let mut m = sample();
        update(&mut m, Msg::Resize(110, 20), now());
        m
    }

    /// A click at `x`, `y`, `ms` milliseconds after `now()`.
    fn click(m: &mut Model, x: u16, y: u16, ms: i64) -> Vec<Effect> {
        let at = now() + SignedDuration::from_millis(ms);
        update(m, Msg::Mouse(Mouse::Click, x, y), at)
    }

    fn wheel(m: &mut Model, action: Mouse, x: u16, y: u16) {
        assert_eq!(update(m, Msg::Mouse(action, x, y), now()), []);
    }

    #[test]
    fn a_click_selects_the_entry_under_it() {
        let mut m = on_screen();
        key(&mut m, KeyCode::Enter);
        // Row 0, its members E01 E02 E03, then row 1: y 3 is E02's member.
        assert_eq!(click(&mut m, 5, 3, 0), []);
        assert_eq!(m.selected_entry(), Some(Entry::Member(0, 1)));
        // The border, the title, below the last entry, the detail pane: nothing.
        for (x, y) in [(0, 2), (5, 0), (5, 9), (70, 2)] {
            click(&mut m, x, y, 1000);
            assert_eq!(m.selected_entry(), Some(Entry::Member(0, 1)), "{x},{y}");
        }
    }

    #[test]
    fn a_click_lands_on_the_entry_drawn_there_when_the_list_is_scrolled() {
        let mut m = crate::review::model::tests_support::many(30);
        update(&mut m, Msg::Resize(110, 20), now());
        key(&mut m, KeyCode::End);
        // 17 rows, ending at entry 29: entry 13 is on the first row.
        assert_eq!(m.scroll, 13);
        click(&mut m, 5, 1, 0);
        assert_eq!(m.selected, 13);
        click(&mut m, 5, 17, 1000);
        assert_eq!(m.selected, 29);
    }

    #[test]
    fn the_first_click_lands_on_the_row_drawn_there_when_review_opens_scrolled() {
        // Review opened on the first pending row, entry 25, before any message: the screen it
        // first draws is scrolled to show it.
        let mut m = crate::review::model::tests_support::many(30);
        m.selected = 25;
        m.set_area(Rect::new(0, 0, 110, 20));
        assert_eq!(m.scroll, 9);
        click(&mut m, 5, 1, 0);
        assert_eq!(m.selected, 9);
    }

    #[test]
    fn a_double_click_opens_and_a_slow_or_moved_one_does_not() {
        let mut m = on_screen();
        let opens = |effects: Vec<Effect>| matches!(effects[..], [Effect::Open(_)]);
        assert_eq!(click(&mut m, 5, 2, 0), []);
        assert!(opens(click(&mut m, 5, 2, 500)), "within 500 ms");
        assert_eq!(m.selected_entry(), Some(Entry::Row(1)));
        // A third click starts over.
        assert_eq!(click(&mut m, 5, 2, 600), []);
        // Too slow.
        assert_eq!(click(&mut m, 5, 2, 2000), []);
        assert_eq!(click(&mut m, 5, 2, 2501), []);
        // Two rows.
        assert_eq!(click(&mut m, 5, 1, 4000), []);
        assert_eq!(click(&mut m, 5, 2, 4100), []);
        // Something in between.
        assert_eq!(click(&mut m, 5, 2, 6000), []);
        key(&mut m, KeyCode::Char('f'));
        assert_eq!(click(&mut m, 5, 2, 6100), []);
        // The clock stepped back between them (NTP, a resume): not a double-click.
        assert_eq!(click(&mut m, 5, 2, 8000), []);
        assert_eq!(click(&mut m, 5, 2, 7000), []);
    }

    #[test]
    fn a_double_click_survives_the_scans_messages_between_its_clicks() {
        let opens = |effects: Vec<Effect>| matches!(effects[..], [Effect::Open(_)]);
        // The spinner's tick, which comes after 250 ms without a message.
        let mut m = on_screen();
        assert_eq!(click(&mut m, 5, 2, 0), []);
        update(&mut m, Msg::Tick, now());
        assert!(opens(click(&mut m, 5, 2, 300)), "a tick in between");
        // A file's findings, a row of them above the snacks row, which moves down a line: the
        // second click lands on it there.
        let mut m = on_screen();
        assert_eq!(click(&mut m, 5, 2, 0), []);
        let msg = saved(
            &m,
            1,
            vec![finding(5, 1, "An erly line.", "An early line.", 1000)],
        );
        update(&mut m, msg, now());
        assert_eq!(m.entries()[2], Entry::Row(2));
        assert!(opens(click(&mut m, 5, 3, 300)), "a row in above it");
    }

    #[test]
    fn the_wheel_moves_the_selection_over_the_list_and_scrolls_the_detail() {
        let mut m = on_screen();
        wheel(&mut m, Mouse::WheelDown, 10, 5);
        assert_eq!(m.selected, 1);
        wheel(&mut m, Mouse::WheelUp, 10, 5);
        assert_eq!(m.selected, 0);
        // The ×3 row's detail is 11 lines: the wheel stops with the last one on top.
        for _ in 0..5 {
            wheel(&mut m, Mouse::WheelDown, 70, 5);
        }
        assert_eq!(m.detail_scroll, 10);
        wheel(&mut m, Mouse::WheelUp, 70, 5);
        assert_eq!(m.detail_scroll, 7);
        // A new selection starts at the top.
        key(&mut m, KeyCode::Down);
        assert_eq!(m.detail_scroll, 0);
    }

    #[test]
    fn a_detail_that_got_shorter_is_scrolled_back_to_its_last_line() {
        let mut m = on_screen();
        // Scrolled for a longer text: a decision or a filter can shorten it while the same entry
        // stays selected.
        m.detail_scroll = 100;
        update(&mut m, Msg::Played(Err("mpv is not open".into())), now());
        assert_eq!(
            m.detail_scroll, 10,
            "the ×3 row's 11 lines, the last on top"
        );
    }

    #[test]
    fn question_mark_shows_the_keys_and_any_key_closes_them() {
        let mut m = on_screen();
        assert!(!m.help);
        press(&mut m, '?');
        assert!(m.help);
        // Closing is all the key does: no decision.
        assert_eq!(press(&mut m, 'a'), []);
        assert!(!m.help);
        assert_eq!(states(&m), [State::Pending; 4]);
        press(&mut m, '?');
        key(&mut m, KeyCode::Esc);
        assert!(!m.help);
        // But `q` quits, as the box says.
        press(&mut m, '?');
        press(&mut m, 'q');
        assert!(m.should_quit);
        m.should_quit = false;
        // `? help` at the end of the status line opens them; a click anywhere closes them, and the
        // wheel does nothing under them.
        click(&mut m, 105, 19, 0);
        assert!(m.help);
        wheel(&mut m, Mouse::WheelDown, 10, 5);
        assert_eq!(m.selected, 0);
        click(&mut m, 5, 2, 1000);
        assert!(!m.help);
        assert_eq!(m.selected, 0, "the click only closed them");
    }

    #[test]
    fn while_editing_only_the_editors_buttons_work() {
        let mut m = on_screen();
        m.selected = 1;
        press(&mut m, 'e');
        assert_eq!(click(&mut m, 5, 1, 0), []);
        wheel(&mut m, Mouse::WheelUp, 5, 1);
        assert_eq!(click(&mut m, 50, 19, 1000), [], "the text");
        assert_eq!(click(&mut m, 98, 19, 1500), [], "the separator");
        assert_eq!(m.selected, 1);
        assert!(m.editor.is_some());
        // "⏎ accept · Esc cancel" at the end of the status line: Esc from x 100.
        click(&mut m, 100, 19, 2000);
        assert!(m.editor.is_none());
        press(&mut m, 'e');
        assert_eq!(
            decided(&click(&mut m, 89, 19, 3000)),
            [(3, State::Accepted, None)]
        );
    }

    #[test]
    fn q_and_ctrl_c_quit() {
        let mut m = sample();
        press(&mut m, 'q');
        assert!(m.should_quit);
        let mut m = sample();
        update(&mut m, Msg::Quit, now());
        assert!(m.should_quit);
    }

    #[test]
    fn a_file_the_scan_saved_is_read_back_and_one_it_failed_keeps_its_findings() {
        let mut m = sample();
        let effects = update(
            &mut m,
            appended(2, vec![finding(9, 2, TYPO, FIXED, 165_060)]),
            now(),
        );
        assert_eq!(effects, [Effect::ReadBack(path(2))]);
        assert_eq!(
            m.run.findings.len(),
            4,
            "its findings come with the read-back"
        );
        let failed = crate::store::FileRecord {
            error: Some("LLM: boom".into()),
            ..tests_support::file(2)
        };
        let effects = update(
            &mut m,
            Msg::Scan(Event::Appended(vec![Record::File(failed)])),
            now(),
        );
        assert_eq!(effects, []);
        assert_eq!(m.run.findings.len(), 4);
        assert_eq!(m.run.files[1].error.as_deref(), Some("LLM: boom"));
    }

    #[test]
    fn a_decision_the_store_found_replaced_reads_its_file_back_and_says_so() {
        let mut m = sample();
        m.selected = 1;
        let effects = press(&mut m, 'a');
        let [Effect::Write { action, decisions }] = <[Effect; 1]>::try_from(effects).unwrap()
        else {
            panic!("expected one write");
        };
        let followed = Msg::Written {
            action: action.clone(),
            decisions: decisions.clone(),
            result: Ok(vec![Decided::Followed(vec![9])]),
        };
        assert_eq!(update(&mut m, followed, now()), [Effect::ReadBack(path(2))]);
        assert_eq!(
            m.status.as_deref(),
            Some("E02 was checked again; your decision went to its new finding")
        );
        let gone = Msg::Written {
            action,
            decisions,
            result: Ok(vec![Decided::Gone]),
        };
        assert_eq!(update(&mut m, gone, now()), [Effect::ReadBack(path(2))]);
        assert_eq!(
            m.status.as_deref(),
            Some("E02 was checked again; that finding is gone")
        );
    }

    #[test]
    fn a_file_that_cannot_be_read_back_says_so_and_stays_as_it_was() {
        let mut m = sample();
        let effects = update(
            &mut m,
            Msg::ReadBack {
                path: path(2),
                view: Err("database is locked".into()),
            },
            now(),
        );
        assert_eq!(effects, []);
        assert_eq!(
            m.status.as_deref(),
            Some("could not read E02 back: database is locked")
        );
        assert_eq!(m.run.findings.len(), 4);
    }

    /// The sample run's files view, on E02.
    fn on_e02_in_files() -> Model {
        let mut m = sample();
        update(&mut m, Msg::Key(KeyCode::Tab), now());
        m.file_selected = 1;
        m
    }

    #[test]
    fn c_on_a_file_finds_its_run_then_asks() {
        let mut m = on_e02_in_files();
        assert_eq!(
            press(&mut m, 'c'),
            [Effect::FindRun {
                path: path(2),
                run: Some("2026-10-17T04-39-18".into()),
            }]
        );
        let found = Msg::RunFound {
            path: path(2),
            found: Ok(("2026-10-17T04-39-18".into(), "claude".into())),
        };
        update(&mut m, found, now());
        assert_eq!(
            m.status_text(Rect::new(0, 0, 160, 1)).as_deref(),
            Some("check E02 again with claude? y/n")
        );
        // Anything but `y` lets it go.
        assert_eq!(press(&mut m, 'n'), []);
        assert_eq!(m.confirm, None);
        // A paths view has no run of its own: the store finds the newest that named the file.
        let mut m = on_e02_in_files();
        m.run.of_paths = true;
        assert_eq!(
            press(&mut m, 'c'),
            [Effect::FindRun {
                path: path(2),
                run: None,
            }]
        );
    }

    #[test]
    fn y_with_no_scan_running_checks_the_file_again_at_once() {
        let mut m = on_e02_in_files();
        m.confirm = Some(Confirm::Recheck {
            path: path(2),
            run: "r".into(),
            backend: "claude".into(),
        });
        assert_eq!(
            press(&mut m, 'y'),
            [Effect::Recheck {
                run: "r".into(),
                paths: vec![path(2)],
            }]
        );
        let job = Job {
            run: "r".into(),
            resume: "subcheck scan --run r --force /m/E02.mkv".into(),
        };
        update(
            &mut m,
            Msg::Started {
                job: Ok(job),
                missing: vec![],
                paths: vec![path(2)],
            },
            now(),
        );
        assert!(m.progress.running());
        assert_eq!(m.progress.run, "r");
    }

    #[test]
    fn y_while_a_scan_runs_queues_the_file_and_the_queue_starts_a_run_at_a_time_when_it_ends() {
        let mut m = scanning();
        for (ep, run) in [(1, "r"), (2, "s"), (3, "r")] {
            m.confirm = Some(Confirm::Recheck {
                path: path(ep),
                run: run.into(),
                backend: "claude".into(),
            });
            assert_eq!(press(&mut m, 'y'), []);
        }
        assert_eq!(
            m.status.as_deref(),
            Some("E03 will be checked again when the scan is done")
        );
        let labels: Vec<String> = m.progress.files[..3].iter().map(|f| f.label('⠙')).collect();
        assert_eq!(labels, ["queued again"; 3]);
        let ended = update(&mut m, Msg::ScanEnded(Ok(scan::Summary::default())), now());
        assert_eq!(
            ended,
            [Effect::Recheck {
                run: "r".into(),
                paths: vec![path(1), path(3)],
            }]
        );
        assert_eq!(m.progress.again, [(path(2), "s".to_string())]);
    }

    #[test]
    fn c_on_a_file_being_checked_or_queued_again_says_so() {
        let mut m = scanning();
        update(&mut m, Msg::Key(KeyCode::Tab), now());
        m.file_selected = 3;
        assert_eq!(m.progress.files[3].path, path(4));
        assert_eq!(press(&mut m, 'c'), []);
        assert_eq!(m.status.as_deref(), Some("E04 is being checked now"));
        m.file_selected = 1;
        m.progress.queue_again(&path(2), "r");
        assert_eq!(press(&mut m, 'c'), []);
        assert_eq!(
            m.status.as_deref(),
            Some("E02 will be checked again when the scan is done")
        );
    }

    #[test]
    fn c_in_the_findings_does_nothing() {
        let mut m = sample();
        assert_eq!(press(&mut m, 'c'), []);
        assert_eq!(m.confirm, None);
    }

    #[test]
    fn a_file_no_longer_on_disk_is_not_checked_and_the_status_line_says_so() {
        let mut m = on_e02_in_files();
        let effects = update(
            &mut m,
            Msg::Started {
                job: Err("nothing to check".into()),
                missing: vec![path(2)],
                paths: vec![path(2)],
            },
            now(),
        );
        assert_eq!(effects, []);
        assert_eq!(m.status.as_deref(), Some("E02: no such file"));
        assert_eq!(m.progress.files[1].label('⠙'), "✗ FAILED: no such file");
        assert!(!m.progress.running());
    }

    #[test]
    fn a_queued_check_that_cannot_start_fails_its_files_and_the_next_run_goes() {
        let mut m = scanning();
        for (ep, run) in [(1, "r"), (2, "s")] {
            m.confirm = Some(Confirm::Recheck {
                path: path(ep),
                run: run.into(),
                backend: "claude".into(),
            });
            assert_eq!(press(&mut m, 'y'), []);
        }
        let ended = update(&mut m, Msg::ScanEnded(Ok(scan::Summary::default())), now());
        assert_eq!(
            ended,
            [Effect::Recheck {
                run: "r".into(),
                paths: vec![path(1)],
            }]
        );
        let why = "run r: no LLM is called gpt";
        let effects = update(
            &mut m,
            Msg::Started {
                job: Err(why.into()),
                missing: vec![],
                paths: vec![path(1)],
            },
            now(),
        );
        assert_eq!(
            effects,
            [Effect::Recheck {
                run: "s".into(),
                paths: vec![path(2)],
            }]
        );
        assert_eq!(m.progress.files[0].label('⠙'), format!("✗ FAILED: {why}"));
        assert!(
            !m.progress.outro().iter().any(|l| l.contains("FAILED")),
            "the earlier scan's outro does not list a file its recheck never started"
        );
        assert_eq!(m.status.as_deref(), Some(why));
        update(&mut m, Msg::Key(KeyCode::Tab), now());
        m.file_selected = 0;
        assert!(matches!(
            press(&mut m, 'c').as_slice(),
            [Effect::FindRun { .. }]
        ));
    }

    #[test]
    fn ctrl_c_over_cs_question_asks_to_quit_first() {
        let mut m = scanning();
        m.confirm = Some(Confirm::Recheck {
            path: path(1),
            run: "r".into(),
            backend: "claude".into(),
        });
        update(&mut m, Msg::Quit, now());
        assert!(!m.should_quit);
        assert_eq!(m.confirm, Some(Confirm::Quit));
        update(&mut m, Msg::Quit, now());
        assert!(m.should_quit);
    }

    #[test]
    fn v_asks_for_a_runs_folders_or_the_run_that_named_the_selected_file() {
        let mut m = sample();
        assert_eq!(
            press(&mut m, 'v'),
            [Effect::Switch(Switch::FoldersOf(m.run.name.clone()))]
        );
        // In a paths view, the selected row's first member's file.
        m.run.of_paths = true;
        m.selected = 0;
        assert_eq!(
            press(&mut m, 'v'),
            [Effect::Switch(Switch::RunNaming(path(1)))]
        );
        // In the files view, the selected file.
        update(&mut m, Msg::Key(KeyCode::Tab), now());
        m.file_selected = 2;
        assert_eq!(
            press(&mut m, 'v'),
            [Effect::Switch(Switch::RunNaming(path(3)))]
        );
    }

    #[test]
    fn v_with_nothing_selected_in_the_list_follows_the_files_selected_file() {
        let mut m = sample();
        m.run.of_paths = true;
        // Nothing in the sample is rejected: under that filter the list is empty.
        m.filter = Filter::Only(State::Rejected);
        m.file_selected = 1;
        assert_eq!(m.selected_entry(), None);
        assert_eq!(
            press(&mut m, 'v'),
            [Effect::Switch(Switch::RunNaming(path(2)))]
        );
    }

    #[test]
    fn v_while_a_scan_runs_says_so_and_asks_for_nothing() {
        let mut m = scanning();
        assert_eq!(press(&mut m, 'v'), []);
        assert_eq!(m.status.as_deref(), Some("the scan is still running"));
    }

    #[test]
    fn a_view_that_cannot_be_read_leaves_the_screen_as_it_was() {
        let mut m = sample();
        let before = m.run.clone();
        update(&mut m, Msg::Switched(Err("disk I/O error".into())), now());
        assert_eq!(m.run, before);
        assert_eq!(
            m.status.as_deref(),
            Some("could not switch views: disk I/O error")
        );
    }

    /// The sample with findings 1 (E01) and 3 (E02) accepted, 3's fix in its file already.
    fn accepted() -> Model {
        let mut m = sample();
        for id in [1, 3] {
            m.run.decisions.insert(
                id,
                Decision {
                    finding: id,
                    state: State::Accepted,
                    edited: None,
                    at: now(),
                },
            );
        }
        m.run.applied.insert(3);
        m
    }

    /// Apply done with the file at `path`.
    fn file_done(
        path: String,
        outcomes: Vec<(u64, Outcome)>,
        marked: Vec<u64>,
        restamped: Option<(u64, Timestamp)>,
    ) -> Msg {
        Msg::Apply(apply::Event::Done {
            path,
            outcomes,
            remuxed: restamped.is_some(),
            marked,
            restamped,
            text: String::new(),
        })
    }

    #[test]
    fn a_asks_with_the_counts_then_applies_the_view_as_it_stands() {
        let mut m = accepted();
        assert_eq!(press(&mut m, 'A'), []);
        assert_eq!(m.confirm, Some(Confirm::Apply));
        assert_eq!(
            m.status_text(Rect::default()).as_deref(),
            Some("apply 2 fixes in 2 files (1 applied already), keeping each .orig? y/n")
        );
        // Any key but y lets it go.
        assert_eq!(press(&mut m, 'n'), []);
        assert_eq!(m.confirm, None);
        assert!(!m.progress.busy());
        press(&mut m, 'A');
        let effects = press(&mut m, 'y');
        assert_eq!(
            effects,
            [Effect::StartApply {
                run: m.run.clone(),
                dry_run: false,
                karaoke: false,
            }]
        );
        assert!(m.progress.applying());
        assert_eq!(
            m.progress.apply.as_ref().unwrap().resume,
            "subcheck apply 2026-10-17T04-39-18"
        );
    }

    #[test]
    fn one_fix_in_one_file_is_asked_of_in_the_singular() {
        let mut m = accepted();
        m.run.decisions.remove(&3);
        press(&mut m, 'A');
        assert_eq!(
            m.status_text(Rect::default()).as_deref(),
            Some("apply 1 fix in 1 file, keeping each .orig? y/n")
        );
    }

    #[test]
    fn d_starts_a_dry_run_at_once_with_the_screens_karaoke() {
        let mut m = accepted();
        m.karaoke = true;
        let effects = press(&mut m, 'D');
        assert_eq!(
            effects,
            [Effect::StartApply {
                run: m.run.clone(),
                dry_run: true,
                karaoke: true,
            }]
        );
        assert_eq!(
            m.progress.apply.as_ref().unwrap().resume,
            "subcheck apply 2026-10-17T04-39-18 --dry-run --karaoke"
        );
    }

    #[test]
    fn a_and_d_wait_for_the_scan_and_for_apply_and_need_an_accepted_fix() {
        let mut m = sample();
        assert_eq!(press(&mut m, 'A'), []);
        assert_eq!(m.status.as_deref(), Some("no accepted fixes"));
        let mut m = accepted();
        m.progress
            .start("r", "subcheck scan --run r /m".into(), false);
        for c in ['A', 'D'] {
            assert_eq!(press(&mut m, c), []);
            assert_eq!(m.status.as_deref(), Some("the scan is still running"));
        }
        let mut m = accepted();
        press(&mut m, 'D');
        for c in ['A', 'D'] {
            assert_eq!(press(&mut m, c), []);
            assert_eq!(m.status.as_deref(), Some("already applying"));
        }
        // From the files view too.
        key(&mut m, KeyCode::Tab);
        assert_eq!(press(&mut m, 'A'), []);
        assert_eq!(m.status.as_deref(), Some("already applying"));
    }

    #[test]
    fn v_and_c_wait_for_apply_and_q_asks_while_it_runs() {
        let mut m = accepted();
        press(&mut m, 'D');
        assert_eq!(press(&mut m, 'v'), []);
        assert_eq!(m.status.as_deref(), Some("apply is still running"));
        key(&mut m, KeyCode::Tab);
        assert_eq!(press(&mut m, 'c'), []);
        assert_eq!(m.status.as_deref(), Some("apply is still running"));
        press(&mut m, 'q');
        assert_eq!(m.confirm, Some(Confirm::Quit));
        update(
            &mut m,
            Msg::Apply(apply::Event::Queued(vec![path(1)])),
            now(),
        );
        update(
            &mut m,
            Msg::Apply(apply::Event::Stage(path(1), apply::Stage::Plan)),
            now(),
        );
        assert_eq!(
            m.status_text(Rect::default()).as_deref(),
            Some("dry run is on E01 (plan), 0/1 — stop it and quit? y/n")
        );
        // Apply ending lets the question go: nothing is left to stop.
        update(
            &mut m,
            Msg::ApplyEnded(Ok(apply::Summary::default())),
            now(),
        );
        assert_eq!(m.confirm, None);
        assert!(!m.should_quit);
    }

    #[test]
    fn a_file_done_marks_its_findings_and_takes_the_stores_marks_and_stamp() {
        let mut m = accepted();
        press(&mut m, 'D');
        update(
            &mut m,
            Msg::Apply(apply::Event::Queued(vec![path(1), path(2)])),
            now(),
        );
        update(
            &mut m,
            file_done(path(1), vec![(1, Outcome::Patched)], vec![], None),
            now(),
        );
        update(
            &mut m,
            file_done(path(2), vec![(3, Outcome::AlreadyApplied)], vec![], None),
            now(),
        );
        assert_eq!(m.marks.get(&1), Some(&Mark::WouldApply));
        // Findings 1 and 3 are at indexes 0 and 2.
        assert_eq!(m.mark(0), Some("would apply"));
        assert_eq!(m.mark(2), Some("already applied"));
        update(
            &mut m,
            Msg::ApplyEnded(Ok(apply::Summary::default())),
            now(),
        );
        // Then for real: E01's fix made, E02's left by hand.
        press(&mut m, 'A');
        press(&mut m, 'y');
        update(
            &mut m,
            Msg::Apply(apply::Event::Queued(vec![path(1), path(2)])),
            now(),
        );
        let stamp: (u64, Timestamp) = (123, "2026-10-30T09:00:00Z".parse().unwrap());
        update(
            &mut m,
            file_done(path(1), vec![(1, Outcome::Patched)], vec![1], Some(stamp)),
            now(),
        );
        update(
            &mut m,
            file_done(
                path(2),
                vec![(3, Outcome::ByHand("a karaoke row".into()))],
                vec![],
                None,
            ),
            now(),
        );
        assert_eq!(m.marks.get(&1), None);
        assert!(m.run.applied.contains(&1));
        assert_eq!(m.mark(0), Some("applied"));
        assert_eq!(m.mark(2), Some("by hand"));
        assert_eq!((m.run.files[0].size, m.run.files[0].mtime), stamp);
        assert_eq!(m.run.files[1].size, 100, "E02 was not swapped");
    }

    #[test]
    fn a_decision_change_drops_a_dry_run_mark_and_keeps_an_applys() {
        let mut m = accepted();
        m.marks.insert(2, Mark::WouldApply);
        m.marks.insert(3, Mark::ByHand("a karaoke row".into()));
        // Row 0 is the OP lyric (findings 1, 2 and 4: `r` rejects the pending 2 and 4); row 1 the
        // snacks typo (3, accepted).
        m.selected = 0;
        let effects = press(&mut m, 'r');
        written(&mut m, effects);
        assert_eq!(m.marks.get(&2), None);
        m.selected = 1;
        let effects = press(&mut m, 'r');
        written(&mut m, effects);
        assert_eq!(m.marks.get(&3), Some(&Mark::ByHand("a karaoke row".into())));
    }

    #[test]
    fn an_apply_that_cannot_start_says_why() {
        let mut m = accepted();
        press(&mut m, 'D');
        update(&mut m, Msg::ApplyStarted(Err("no threads".into())), now());
        assert!(!m.progress.busy());
        assert_eq!(
            m.status_text(Rect::default()).as_deref(),
            Some("apply failed: no threads")
        );
    }
}
