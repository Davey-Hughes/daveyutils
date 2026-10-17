//! Keys to state changes, and the effects the event loop runs for them. Pure: the run file and mpv
//! are touched only by the effects, so every rule here is tested without a terminal.

use crossterm::event::KeyCode;
use jiff::Timestamp;

use super::model::{Editor, Entry, FileNote, Model, UndoStep};
use crate::list::{self, label, Filter};
use crate::player::Moment;
use crate::store::{Decision, State};
use crate::text::timestamp;

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Key(KeyCode),
    /// Ctrl-U: in the editor, delete back to the start.
    KillLine,
    Quit,
    /// A `Write` effect finished.
    Written {
        action: Action,
        decisions: Vec<Decision>,
        result: Result<(), String>,
    },
    /// An `Open` effect finished.
    Opened(Result<(), String>),
}

/// What a write was for, so its result can be applied.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Decide { entry: Entry, before: UndoStep },
    Undo,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Append these decisions to the run file.
    Write {
        action: Action,
        decisions: Vec<Decision>,
    },
    Open(Moment),
}

/// Rows a PgUp or PgDn moves.
const PAGE: usize = 10;

pub fn update(m: &mut Model, msg: Msg, now: Timestamp) -> Vec<Effect> {
    match msg {
        Msg::Quit => {
            m.should_quit = true;
            Vec::new()
        }
        Msg::KillLine => {
            if let Some(ed) = &mut m.editor {
                ed.text = ed.text.chars().skip(ed.cursor).collect();
                ed.cursor = 0;
            }
            Vec::new()
        }
        Msg::Key(key) if m.editor.is_some() => edit_key(m, key, now),
        Msg::Key(key) => {
            m.status = None;
            key_press(m, key, now)
        }
        Msg::Written {
            action,
            decisions,
            result,
        } => {
            written(m, action, &decisions, result);
            Vec::new()
        }
        Msg::Opened(result) => {
            m.status = Some(match result {
                Ok(()) => "opened in mpv".into(),
                Err(e) => e,
            });
            Vec::new()
        }
    }
}

fn key_press(m: &mut Model, key: KeyCode, now: Timestamp) -> Vec<Effect> {
    let last = m.entries().len().saturating_sub(1);
    match key {
        KeyCode::Char('q') => m.should_quit = true,
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
                let text = m.wording(m.targets(entry)[0]).to_string();
                let cursor = text.chars().count();
                m.editor = Some(Editor {
                    entry,
                    text,
                    cursor,
                });
            }
        }
        KeyCode::Char('u') => return undo(m, now),
        KeyCode::Char('o') => return open(m),
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
            let reworded = ed.text != m.finding(m.targets(ed.entry)[0]).suggestion;
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

/// Decisions for each finding `entry` stands for that is not already `state` with that wording.
fn decide(
    m: &mut Model,
    entry: Entry,
    state: State,
    edited: Option<&str>,
    now: Timestamp,
) -> Vec<Effect> {
    let mut before = Vec::new();
    let mut decisions = Vec::new();
    for i in m.targets(entry) {
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
    if decisions.is_empty() {
        m.status = Some(format!("already {}", label(state)));
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

/// A write's result: on success the decisions take effect, and the selection moves on.
fn written(m: &mut Model, action: Action, decisions: &[Decision], result: Result<(), String>) {
    if let Err(e) = result {
        m.status = Some(format!("not saved: {e}"));
        return;
    }
    let old = m.selected;
    let shown = m.entries();
    for d in decisions {
        m.run.decisions.insert(d.finding, d.clone());
    }
    let n = decisions.len();
    let what = if n == 1 {
        "1 finding".to_string()
    } else {
        format!("{n} findings")
    };
    match action {
        Action::Decide { entry, before } => {
            // `a` means the suggestion as written, so it replaces a rewording; say so, since `u`
            // can bring it back.
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
            m.status = Some(format!("{} {what}{replacing}", label(decisions[0].state)));
            m.undo.push(before);
            advance(m, entry, old, &shown);
        }
        Action::Undo => {
            m.undo.pop();
            m.status = Some(format!("undone: {what}"));
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
}

/// After a decision on `decided`, which was at `old` in `shown` (the list before it): select the
/// first entry after it that is still shown, passing over a decided row's members. The decided
/// entry may be gone, and with it more than itself: the last shown member of a group takes the
/// group's row along.
fn advance(m: &mut Model, decided: Entry, old: usize, shown: &[Entry]) {
    let now = m.entries();
    let next = shown
        .iter()
        .skip(old + 1)
        .filter(|e| {
            !(matches!(decided, Entry::Row(_))
                && matches!(e, Entry::Member(r, _) if *r == decided.row()))
        })
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
        Filter::Only(State::Pending) => Filter::All,
        Filter::All => Filter::Only(State::Accepted),
        Filter::Only(State::Accepted) => Filter::Only(State::Rejected),
        Filter::Only(State::Rejected) => Filter::Only(State::Pending),
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
    let (start_ms, sid) = list::mpv_moment(&m.run, f);
    let moment = Moment {
        file: f.file.clone(),
        start_ms,
        sid,
    };
    m.status = Some(format!(
        "opening {} at {}",
        m.label(&moment.file),
        timestamp(start_ms)
    ));
    vec![Effect::Open(moment)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::model::tests_support::{path, sample};

    fn now() -> Timestamp {
        "2026-10-17T12:00:00Z".parse().unwrap()
    }

    fn key(m: &mut Model, code: KeyCode) -> Vec<Effect> {
        update(m, Msg::Key(code), now())
    }

    fn press(m: &mut Model, c: char) -> Vec<Effect> {
        key(m, KeyCode::Char(c))
    }

    /// Run a Write effect as if the run file took it, and feed the result back.
    fn written(m: &mut Model, effects: Vec<Effect>) {
        let [Effect::Write { action, decisions }] = <[Effect; 1]>::try_from(effects).unwrap()
        else {
            panic!("expected one write");
        };
        update(
            m,
            Msg::Written {
                action,
                decisions,
                result: Ok(()),
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
        // Under the pending filter the decided row is gone; the next one took its place.
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
        assert_eq!(m.status.as_deref(), Some("undone: 3 findings"));
        assert_eq!(press(&mut m, 'u'), []);
        assert_eq!(m.status.as_deref(), Some("nothing to undo"));
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
        m.selected = 1;
        press(&mut m, 'f');
        assert_eq!(m.filter, Filter::All);
        assert_eq!(m.selected_entry(), Some(Entry::Row(1)));
        press(&mut m, 'f');
        assert_eq!(m.filter, Filter::Only(State::Accepted));
        assert_eq!(m.entries(), []);
        press(&mut m, 'f');
        press(&mut m, 'f');
        assert_eq!(m.filter, Filter::Only(State::Pending));
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
            })]
        );
        assert_eq!(m.status.as_deref(), Some("opening E01 at 0:02:43.06"));
        update(&mut m, Msg::Opened(Err("mpv not found".into())), now());
        assert_eq!(m.status.as_deref(), Some("mpv not found"));
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
    fn q_and_ctrl_c_quit() {
        let mut m = sample();
        press(&mut m, 'q');
        assert!(m.should_quit);
        let mut m = sample();
        update(&mut m, Msg::Quit, now());
        assert!(m.should_quit);
    }
}
