//! Review state: a run's findings grouped into rows, what the list shows, and what is under way.

use std::collections::hash_map::Entry as Slot;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

use jiff::Timestamp;
use ratatui::layout::Rect;

use super::layout::{self, EDIT_KEYS, HELP, Item};
use crate::list::{self, Filter};
use crate::llm::Spent;
use crate::store::{Decision, Finding, Run, State, TrackRecord};
use crate::timing::frame_count;

/// Findings decided together, as one row. Repeats are common: typos with the same original and
/// suggestion (an OP lyric is a finding in every episode), and timing findings on the same-named
/// track of one folder (a season's track), whatever each member's shift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Indexes into `run.findings`, in file order, then time order.
    pub members: Vec<usize>,
}

/// One line of the list: a row, or one member of an expanded row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Entry {
    Row(usize),
    /// The row, and the member's index into `run.findings`.
    Member(usize, usize),
}

impl Entry {
    pub fn row(self) -> usize {
        match self {
            Entry::Row(r) | Entry::Member(r, _) => r,
        }
    }
}

/// How a file differs from when it was scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileNote {
    Changed,
    Missing,
}

/// The one-line editor that `e` opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
    pub entry: Entry,
    pub text: String,
    /// In chars.
    pub cursor: usize,
}

/// What one `u` takes back: each finding a decision changed, with the decision it had before.
pub type UndoStep = Vec<(u64, Option<Decision>)>;

pub struct Model {
    pub run: Run,
    pub rows: Vec<Row>,
    /// Each finding's [`track_name`], by index; the list draws them on every redraw.
    track_names: Vec<String>,
    pub expanded: HashSet<usize>,
    pub filter: Filter,
    /// Whether the box of keys `?` opens is showing.
    pub help: bool,
    /// Index into `entries()`.
    pub selected: usize,
    pub editor: Option<Editor>,
    /// Decisions made in this session, oldest first.
    pub undo: Vec<UndoStep>,
    /// Files that are not as they were scanned, by path.
    pub notes: HashMap<String, FileNote>,
    pub status: Option<String>,
    /// Whether the status line, with no message of its own, says what the run's LLM calls cost:
    /// from opening until the first key or click.
    pub show_cost: bool,
    pub should_quit: bool,
    /// The name each file goes by in the list.
    pub labels: HashMap<String, String>,
    /// The screen, as the terminal last reported it: where a click lands depends on it.
    pub area: Rect,
    /// The list's first shown entry.
    pub scroll: usize,
    /// How far the wheel has scrolled the detail pane, in lines; back to 0 on a new selection.
    pub detail_scroll: u16,
    /// The last click on the list, while it could still be the first of a double-click.
    pub last_click: Option<(Entry, Timestamp)>,
}

impl Model {
    /// Every finding, decided or not, with the first row that has a pending one selected: a
    /// half-reviewed run opens where the work is.
    pub fn new(run: Run, notes: HashMap<String, FileNote>) -> Model {
        let (rows, track_names) = group(&run);
        let labels = labels(&run);
        let mut m = Model {
            run,
            rows,
            track_names,
            labels,
            expanded: HashSet::new(),
            filter: Filter::All,
            help: false,
            selected: 0,
            editor: None,
            undo: Vec::new(),
            notes,
            status: None,
            show_cost: true,
            should_quit: false,
            area: Rect::default(),
            scroll: 0,
            detail_scroll: 0,
            last_click: None,
        };
        m.selected = m
            .entries()
            .iter()
            .position(|&e| m.has_pending(e))
            .unwrap_or(0);
        m
    }

    /// Whether any finding an entry stands for is pending.
    pub fn has_pending(&self, e: Entry) -> bool {
        self.targets(e)
            .iter()
            .any(|&i| self.state(i) == State::Pending)
    }

    /// A screen of `area`, scrolled to show the selection as the view will draw it: a click maps
    /// rows with `scroll` before any message has moved it.
    pub fn set_area(&mut self, area: Rect) {
        self.area = area;
        self.scroll = self.list_scroll(area);
    }

    /// The list's scroll offset on a screen of `area`: `scroll`, moved just enough to show the
    /// selection. The view draws with it and `update` keeps it.
    pub fn list_scroll(&self, area: Rect) -> usize {
        let rows = layout::list_rows(&layout::panes(area)).height;
        let len = self.entries().len();
        layout::keep_in_view(self.scroll, self.selected, usize::from(rows), len)
    }

    /// The buttons at the end of the status line `status`, when they show there: the editor's while
    /// it is open, beside its prompt and [`layout::EDIT_FIELD`] columns of text; else `? help`,
    /// beside the message.
    pub fn buttons(&self, status: Rect) -> Option<&'static [Item]> {
        let (items, left) = match &self.editor {
            Some(_) => (
                EDIT_KEYS,
                layout::width(layout::PROMPT) + layout::EDIT_FIELD,
            ),
            None => (
                HELP,
                self.status_text(status).map_or(0, |s| layout::width(&s)),
            ),
        };
        layout::right_shown(status, items, left).then_some(items)
    }

    /// What the status line `status` says: its message, else what the run's LLM calls cost while
    /// [`Model::show_cost`] holds, as much of it as fits beside `? help`.
    pub fn status_text(&self, status: Rect) -> Option<String> {
        match &self.status {
            Some(message) => Some(message.clone()),
            None if self.show_cost => cost_status(&self.run.spent, status),
            None => None,
        }
    }

    /// The name a file goes by in the list (see [`labels`]).
    pub fn label(&self, file: &str) -> String {
        self.labels
            .get(file)
            .cloned()
            .unwrap_or_else(|| episode(file))
    }

    pub fn finding(&self, i: usize) -> &Finding {
        &self.run.findings[i]
    }

    pub fn state(&self, i: usize) -> State {
        self.run.state_of(self.finding(i).id)
    }

    /// The wording a finding would be fixed with: the reviewer's edit, else the suggestion.
    pub fn wording(&self, i: usize) -> &str {
        let f = self.finding(i);
        self.run
            .decisions
            .get(&f.id)
            .and_then(|d| d.edited.as_deref())
            .unwrap_or(&f.suggestion)
    }

    fn shows(&self, i: usize) -> bool {
        match self.filter {
            Filter::All => true,
            Filter::Only(state) => self.state(i) == state,
        }
    }

    /// The row's members the filter shows: what a decision on the row applies to.
    pub fn shown_members(&self, row: usize) -> Vec<usize> {
        self.rows[row]
            .members
            .iter()
            .copied()
            .filter(|&i| self.shows(i))
            .collect()
    }

    /// The list as shown: each row with a member the filter shows, and under an expanded row its
    /// shown members.
    pub fn entries(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        for (r, row) in self.rows.iter().enumerate() {
            let shown = self.shown_members(r);
            if shown.is_empty() {
                continue;
            }
            out.push(Entry::Row(r));
            if row.members.len() > 1 && self.expanded.contains(&r) {
                out.extend(shown.into_iter().map(|i| Entry::Member(r, i)));
            }
        }
        out
    }

    /// The findings an entry stands for.
    pub fn targets(&self, e: Entry) -> Vec<usize> {
        match e {
            Entry::Row(r) => self.shown_members(r),
            Entry::Member(_, i) => vec![i],
        }
    }

    /// What `a` `r` `e` on an entry decide: of its targets, the pending ones; all of them when none
    /// is, so that a decided group still flips as a whole. A member decided on its own keeps its
    /// decision when its row is decided later.
    pub fn to_decide(&self, e: Entry) -> Vec<usize> {
        let targets = self.targets(e);
        let pending: Vec<usize> = targets
            .iter()
            .copied()
            .filter(|&i| self.state(i) == State::Pending)
            .collect();
        if pending.is_empty() { targets } else { pending }
    }

    pub fn selected_entry(&self) -> Option<Entry> {
        self.entries().get(self.selected).copied()
    }

    /// The finding the detail pane shows and `o` opens: the selected member, or the selected row's
    /// first shown member.
    pub fn focus(&self) -> Option<usize> {
        self.selected_entry()
            .and_then(|e| self.targets(e).first().copied())
    }

    /// The state of the members the filter shows, when they agree; `None` when they do not.
    pub fn shown_state(&self, row: usize) -> Option<State> {
        let mut states = self.shown_members(row).into_iter().map(|i| self.state(i));
        let first = states.next()?;
        states.all(|s| s == first).then_some(first)
    }

    /// All members' state when they agree; `None` when they do not.
    pub fn row_state(&self, row: usize) -> Option<State> {
        let mut states = self.rows[row].members.iter().map(|&i| self.state(i));
        let first = states.next()?;
        states.all(|s| s == first).then_some(first)
    }

    /// Findings pending, accepted and rejected.
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut n = (0, 0, 0);
        for i in 0..self.run.findings.len() {
            match self.state(i) {
                State::Pending => n.0 += 1,
                State::Accepted => n.1 += 1,
                State::Rejected => n.2 += 1,
            }
        }
        n
    }

    pub fn clamp(&mut self) {
        self.selected = self.selected.min(self.entries().len().saturating_sub(1));
    }

    /// The file an entry belongs to: its first target's.
    pub fn file_of(&self, e: Entry) -> Option<&str> {
        self.targets(e)
            .first()
            .map(|&i| self.finding(i).file.as_str())
    }

    /// Where a finding's line starts, in ms: the occurrence `list --mpv` would open.
    pub fn start_ms(&self, i: usize) -> i64 {
        list::shown_at(&self.run, self.finding(i))
            .0
            .map_or(0, |o| o.start_ms)
    }

    /// A timing finding's track (see [`track_name`]).
    pub fn track_name(&self, i: usize) -> &str {
        &self.track_names[i]
    }

    /// The fixes of timing findings `members` in short, the most common first: `−1 frame ×23 ·
    /// partial ×1`, or one `−1 frame` when they all agree.
    pub fn fix_summary(&self, members: &[usize]) -> String {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for &i in members {
            let label = fix_label(self.finding(i));
            match counts.iter_mut().find(|(l, _)| *l == label) {
                Some((_, n)) => *n += 1,
                None => counts.push((label, 1)),
            }
        }
        if let [(only, _)] = counts.as_slice() {
            return only.clone();
        }
        // Stable: equal counts keep the order they came in.
        counts.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        let parts: Vec<String> = counts.iter().map(|(l, n)| format!("{l} ×{n}")).collect();
        parts.join(" · ")
    }
}

/// A timing finding's track among its file record's `tracks`: its name, `English Full`; with its ID
/// before it when the name is not its own (no name, `track 6 (eng)`, or one another track of the
/// file shares, `track 6 English`), so that it is told apart in the list as in its row; else its
/// ID alone. Empty for other findings.
pub fn track_name(tracks: &[TrackRecord], f: &Finding) -> String {
    let Some(t) = &f.timing else {
        return String::new();
    };
    let id = t.track;
    match list::track_name_in(tracks, id) {
        Some((name, true)) => name,
        Some((name, false)) => format!("track {id} {name}"),
        None => format!("track {id}"),
    }
}

/// Each finding's [`track_name`], by index, through one look-up of the files by path.
fn track_names(run: &Run) -> Vec<String> {
    let by_path: HashMap<&str, &[TrackRecord]> = run
        .files
        .iter()
        .map(|r| (r.path.as_str(), r.tracks.as_slice()))
        .collect();
    run.findings
        .iter()
        .map(|f| track_name(by_path.get(f.file.as_str()).copied().unwrap_or(&[]), f))
        .collect()
}

/// A timing finding's fix in a word: `−1 frame` (its lines moved a frame earlier), `+2 frames`,
/// `partial`. Empty for other findings.
pub fn fix_label(f: &Finding) -> String {
    match f.timing.as_ref().map(|t| t.stretches.as_slice()) {
        Some([whole]) => {
            let sign = if whole.frames > 0 { '−' } else { '+' };
            format!("{sign}{}", frame_count(whole.frames))
        }
        Some(_) => "partial".into(),
        None => String::new(),
    }
}

/// What puts findings in one row (see [`Row`]).
#[derive(PartialEq, Eq, Hash)]
enum Key<'a> {
    Text(&'a str, &'a str),
    /// A folder and a [`track_name`], which carries the ID where the name alone is not the track's.
    Track(&'a str, &'a str),
}

/// The run's findings as rows (see [`Row`]), each where its first member is; and each finding's
/// [`track_name`], by index, which timing findings are grouped by.
pub fn group(run: &Run) -> (Vec<Row>, Vec<String>) {
    let names = track_names(run);
    let mut order: Vec<usize> = (0..run.findings.len()).collect();
    order.sort_by_cached_key(|&i| {
        let f = &run.findings[i];
        let start = list::shown_at(run, f).0.map_or(0, |o| o.start_ms);
        (f.file.clone(), start, f.id)
    });
    let mut rows: Vec<Row> = Vec::new();
    let mut by_key: HashMap<Key, usize> = HashMap::new();
    for i in order {
        let f = &run.findings[i];
        let key = if f.timing.is_some() {
            let folder = f.file.rsplit_once('/').map_or("", |(folder, _)| folder);
            Key::Track(folder, names[i].as_str())
        } else {
            Key::Text(f.original.as_str(), f.suggestion.as_str())
        };
        match by_key.entry(key) {
            Slot::Occupied(slot) => rows[*slot.get()].members.push(i),
            Slot::Vacant(slot) => {
                slot.insert(rows.len());
                rows.push(Row { members: vec![i] });
            }
        }
    }
    (rows, names)
}

/// The `E02` in a file's stem: `E` and digits, as in `- E02 -` or `S01E02`.
fn episode_token(stem: &str) -> Option<Range<usize>> {
    let bytes = stem.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'E' {
            continue;
        }
        let digits = bytes[i + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        let before = i.checked_sub(1).map(|j| bytes[j]);
        let after = bytes.get(i + 1 + digits);
        // `E` starts the token, or follows a season number (`S01E02`).
        let starts = before.is_none_or(|c| !c.is_ascii_alphanumeric())
            || (before.is_some_and(|c| c.is_ascii_digit()) && stem[..i].contains('S'));
        if digits > 0 && starts && after.is_none_or(|c| !c.is_ascii_alphanumeric()) {
            return Some(i..i + 1 + digits);
        }
    }
    None
}

fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// A file's name in the list: its episode, `E02`, else the start of its file name.
pub fn episode(path: &str) -> String {
    let stem = stem(path);
    match episode_token(stem) {
        Some(token) => stem[token].to_string(),
        None => stem.chars().take(12).collect(),
    }
}

/// A file's name when episodes alone would be ambiguous: its file name up to the episode,
/// `K-On!! - E02`, else the start of its file name.
pub fn long_episode(path: &str) -> String {
    let stem = stem(path);
    match episode_token(stem) {
        Some(token) => stem[..token.end].to_string(),
        None => stem.chars().take(24).collect(),
    }
}

/// A file's longer name with its folder before it, `S2/Show - E01`, for seasons kept in folders
/// of their own under the same file names.
fn folder_episode(path: &str) -> String {
    let folder = path.rsplit('/').nth(1).unwrap_or("");
    format!("{folder}/{}", long_episode(path))
}

/// A name per file of the run: the episode; when two files would share one (a run over two seasons
/// has two E01s), the file name up to the episode, for every file; when even those collide, that
/// with its folder.
pub fn labels(run: &Run) -> HashMap<String, String> {
    let mut files: Vec<&str> = run.findings.iter().map(|f| f.file.as_str()).collect();
    files.sort_unstable();
    files.dedup();
    let distinct = |name: fn(&str) -> String| {
        files.iter().map(|f| name(f)).collect::<HashSet<_>>().len() == files.len()
    };
    let name: fn(&str) -> String = if distinct(episode) {
        episode
    } else if distinct(long_episode) {
        long_episode
    } else {
        folder_episode
    };
    files
        .into_iter()
        .map(|f| (f.to_string(), name(f)))
        .collect()
}

/// What differs between two wordings, as byte ranges of each: what remains once the words they
/// share at both ends are set aside. When one side would be empty (a word only added or only
/// removed), a word of context is kept, so both sides show something.
pub fn changed(a: &str, b: &str) -> (Range<usize>, Range<usize>) {
    let (wa, wb) = (words(a), words(b));
    if wa.is_empty() || wb.is_empty() {
        return (0..a.len(), 0..b.len());
    }
    let same = |i: usize, j: usize| a[wa[i].clone()] == b[wb[j].clone()];
    let shortest = wa.len().min(wb.len());
    let mut prefix = (0..shortest).take_while(|&k| same(k, k)).count();
    let mut suffix = (0..shortest - prefix)
        .take_while(|&k| same(wa.len() - 1 - k, wb.len() - 1 - k))
        .count();
    if wa.len() == wb.len() && prefix == wa.len() {
        return (0..0, 0..0);
    }
    if prefix + suffix == shortest {
        if prefix > 0 {
            prefix -= 1;
        } else {
            suffix -= 1;
        }
    }
    let span = |w: &[Range<usize>]| w[prefix].start..w[w.len() - 1 - suffix].end;
    (span(&wa), span(&wb))
}

/// The byte range of each whitespace-separated word.
fn words(s: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(st)) => {
                out.push(st..i);
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(st) = start {
        out.push(st..s.len());
    }
    out
}

/// What the run's LLM calls cost, for the status line `status`: all of it if it fits beside
/// `? help`, else without the tokens, else the price alone, which is what it is there for. The
/// shortest when none fits. `None` with no calls recorded.
fn cost_status(spent: &Spent, status: Rect) -> Option<String> {
    let calls = spent.calls_part()?;
    let (tokens, price) = (spent.tokens_part(), spent.price_part());
    let shapes = [
        vec![Some(calls.clone()), tokens, price.clone()],
        vec![Some(calls), price.clone()],
        vec![price],
    ];
    let lines: Vec<String> = shapes
        .into_iter()
        .filter_map(|parts| {
            let parts: Vec<String> = parts.into_iter().flatten().collect();
            (!parts.is_empty()).then(|| format!("LLM: {}", parts.join(" · ")))
        })
        .collect();
    lines
        .iter()
        .find(|line| layout::right_shown(status, HELP, layout::width(line)))
        .or(lines.last())
        .cloned()
}

/// A small run for the review's tests: the OP lyric "trough" in E01, E02 and E03, and one other
/// finding in E02. Four findings, two rows.
#[cfg(test)]
pub mod tests_support {
    use super::{Model, Row};
    use crate::store::{
        Basis, Check, FileRecord, Finding, FixStretch, LineRef, Run, TimingFix, TrackRecord,
    };
    use crate::text::Occurrence;

    pub fn path(ep: u32) -> String {
        format!("/m/K-On!/K-On! - E{ep:02} - Episode.mkv")
    }

    pub fn file(ep: u32) -> FileRecord {
        let track = |id, sid, name: &str, default| TrackRecord {
            id,
            name: Some(name.into()),
            language: "eng".into(),
            language_bcp47: None,
            sid,
            default,
            read: "cues".into(),
        };
        FileRecord {
            path: path(ep),
            size: 100,
            mtime: jiff::Timestamp::UNIX_EPOCH,
            error: None,
            tracks: vec![
                track(5, 1, "English Full", true),
                track(6, 2, "English Signs", false),
            ],
            lines: 400,
            truncated: None,
            checks: vec![Check::Llm],
        }
    }

    pub fn finding(id: u64, ep: u32, original: &str, suggestion: &str, at_ms: i64) -> Finding {
        Finding {
            id,
            file: path(ep),
            style: if original.contains("trough") {
                "OP en".into()
            } else {
                "Dialogue".into()
            },
            original: original.into(),
            suggestion: suggestion.into(),
            reason: if original.contains("trough") {
                "misspelling of \"through\"".into()
            } else {
                "\"everyday\" should be \"every day\"".into()
            },
            source: "llm".into(),
            occurrences: vec![Occurrence {
                track: 5,
                start_ms: at_ms,
                end_ms: at_ms + 2000,
            }],
            before: vec!["The line before.".into()],
            after: vec!["The line after.".into()],
            timing: None,
        }
    }

    pub fn run() -> Run {
        let typo = "The beat gets trough to our mind";
        let fixed = "The beat gets through to our mind";
        Run {
            name: "2026-10-17T04-39-18".into(),
            files: vec![file(1), file(2), file(3)],
            findings: vec![
                finding(1, 1, typo, fixed, 165_060),
                finding(2, 2, typo, fixed, 165_060),
                finding(
                    3,
                    2,
                    "Let's have snacks together everyday.",
                    "Let's have snacks together every day.",
                    894_190,
                ),
                finding(4, 3, typo, fixed, 165_060),
            ],
            ..Run::default()
        }
    }

    /// A timing finding on episode `ep`'s English Full track (track 5): a frame late, or partial
    /// (late, in sync, late) as K-On!! E16 is.
    pub fn timing(id: u64, ep: u32, partial: bool) -> Finding {
        let line = |start_ms: i64| LineRef {
            raw: format!("0,Dialogue,,0,0,0,,Line at {start_ms}."),
            start_ms,
            end_ms: start_ms + 2000,
        };
        let stretch = |from_ms, to_ms, frames, lines: Option<(i64, i64)>, frame_ms| FixStretch {
            from_ms,
            to_ms,
            frames,
            first: lines.map(|(first, _)| line(first)),
            last: lines.map(|(_, last)| line(last)),
            frame_ms,
        };
        let (original, suggestion, stretches, evidence) = if partial {
            (
                "partial: 1 frame late, in sync, 1 frame late",
                "shift 1 frame earlier: start – 0:07:55.78, 0:15:40.47 – end",
                vec![
                    stretch(None, Some(475_780), 1, Some((1000, 473_780)), Some(246_788)),
                    stretch(Some(483_510), Some(939_000), 0, None, None),
                    stretch(
                        Some(940_470),
                        None,
                        1,
                        Some((940_470, 1_400_000)),
                        Some(1_196_153),
                    ),
                ],
                vec![245_000, 1_195_000],
            )
        } else {
            (
                "1 frame late",
                "shift 1 frame earlier",
                vec![stretch(
                    None,
                    None,
                    1,
                    Some((1000, 1_400_000)),
                    Some(763_763),
                )],
                vec![762_000],
            )
        };
        Finding {
            id,
            file: path(ep),
            style: String::new(),
            original: original.into(),
            suggestion: suggestion.into(),
            reason: "cuts: 345 times, +1 in 5 of 9 windows".into(),
            source: "timing".into(),
            occurrences: evidence
                .into_iter()
                .map(|start_ms| Occurrence {
                    track: 5,
                    start_ms,
                    end_ms: start_ms + 1800,
                })
                .collect(),
            before: vec![],
            after: vec![],
            timing: Some(TimingFix {
                track: 5,
                frame_ns: 41_708_333,
                stretches,
                unshifted: if partial {
                    vec![line(1_300_000)]
                } else {
                    vec![]
                },
                basis: Basis::Cuts {
                    times: 345,
                    agreeing: 5,
                    windows: 9,
                },
            }),
        }
    }

    /// The sample run and timing findings on its English Full track: a frame late in E01 and E03,
    /// partial in E02.
    pub fn with_timing() -> Model {
        let mut run = run();
        run.findings.extend([
            timing(11, 1, false),
            timing(12, 2, true),
            timing(13, 3, false),
        ]);
        Model::new(run, std::collections::HashMap::new())
    }

    /// `n` findings, one per episode, no two alike: `n` rows.
    pub fn many(n: u32) -> Model {
        let run = Run {
            name: "many".into(),
            files: (1..=n).map(file).collect(),
            findings: (1..=n)
                .map(|ep| {
                    let original = format!("Line {ep} has a typo.");
                    let fixed = format!("Line {ep} has no typo.");
                    finding(ep.into(), ep, &original, &fixed, 1000)
                })
                .collect(),
            ..Run::default()
        };
        Model::new(run, std::collections::HashMap::new())
    }

    pub fn sample() -> Model {
        let m = Model::new(run(), std::collections::HashMap::new());
        assert_eq!(
            m.rows,
            [
                Row {
                    members: vec![0, 1, 3]
                },
                Row { members: vec![2] }
            ]
        );
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FileRecord;

    fn label(a: &str, b: &str) -> String {
        let (ra, rb) = changed(a, b);
        format!("{} → {}", &a[ra], &b[rb])
    }

    #[test]
    fn changed_words_are_set_apart_from_the_shared_ends() {
        assert_eq!(
            label(
                "The beat gets trough to our mind",
                "The beat gets through to our mind"
            ),
            "trough → through"
        );
        assert_eq!(
            label(
                "Let's have snacks together everyday.",
                "Let's have snacks together every day."
            ),
            "everyday. → every day."
        );
        // Only removed, or only added: one word of context keeps both sides.
        assert_eq!(
            label(
                "We're a making chocolate cake.",
                "We're making chocolate cake."
            ),
            "We're a → We're"
        );
        assert_eq!(
            label(
                "We happen share the same route",
                "We happen to share the same route"
            ),
            "happen → happen to"
        );
        assert_eq!(label("Wel...", "Well..."), "Wel... → Well...");
        assert_eq!(label("a b", "x a b"), "a → x a");
        assert_eq!(changed("same words", "same  words"), (0..0, 0..0));
        assert_eq!(changed("", "new"), (0..0, 0..3));
    }

    #[test]
    fn a_seasons_timing_findings_on_one_track_are_one_row_whatever_their_fix() {
        let m = tests_support::with_timing();
        // The typo rows as before, and the three episodes' track 5 in one row, at its first member.
        assert_eq!(
            m.rows,
            [
                Row {
                    members: vec![0, 1, 3]
                },
                Row {
                    members: vec![4, 5, 6]
                },
                Row { members: vec![2] },
            ]
        );
        assert_eq!(m.track_name(4), "English Full");
        assert_eq!(m.fix_summary(&[4, 5, 6]), "−1 frame ×2 · partial ×1");
        assert_eq!(m.fix_summary(&[4, 6]), "−1 frame");
        // Another folder's track of the same name is a row of its own.
        let mut other = tests_support::with_timing();
        let path = "/m/K-On!!/K-On!! - E03 - Episode.mkv";
        other.run.findings[6].file = path.into();
        other.run.files.push(FileRecord {
            path: path.into(),
            ..tests_support::file(3)
        });
        assert_eq!(group(&other.run).0.len(), 4);
    }

    #[test]
    fn a_files_tracks_that_go_by_one_name_are_rows_apart_with_their_ids() {
        // Tracks 5 and 6 both unnamed (both `eng`), then both named "English".
        for name in [None, Some("English")] {
            let mut m = tests_support::with_timing();
            for file in &mut m.run.files {
                for t in &mut file.tracks {
                    t.name = name.map(Into::into);
                }
            }
            // E01's track 6, a frame late too.
            let mut signs = tests_support::timing(14, 1, false);
            if let Some(t) = &mut signs.timing {
                t.track = 6;
            }
            m.run.findings.push(signs);
            let (rows, names) = group(&m.run);
            let timing_rows: Vec<Vec<usize>> = rows
                .into_iter()
                .map(|r| r.members)
                .filter(|members| members.iter().all(|&i| m.run.findings[i].timing.is_some()))
                .collect();
            assert_eq!(timing_rows, [vec![4, 5, 6], vec![7]], "{name:?}");
            let labels = [4, 7].map(|i| names[i].as_str());
            match name {
                None => assert_eq!(labels, ["track 5 (eng)", "track 6 (eng)"]),
                Some(_) => assert_eq!(labels, ["track 5 English", "track 6 English"]),
            }
        }
    }

    #[test]
    fn a_fix_in_a_word_counts_the_frames_its_lines_move() {
        let mut f = tests_support::timing(1, 1, false);
        assert_eq!(fix_label(&f), "−1 frame");
        if let Some(t) = &mut f.timing {
            t.stretches[0].frames = -2;
        }
        assert_eq!(fix_label(&f), "+2 frames");
        assert_eq!(fix_label(&tests_support::timing(1, 1, true)), "partial");
        assert_eq!(fix_label(&tests_support::finding(1, 1, "a", "b", 0)), "");
    }

    #[test]
    fn labels_grow_when_two_files_share_an_episode() {
        let one_show = tests_support::run();
        assert_eq!(labels(&one_show)[&tests_support::path(2)], "E02");
        // A second season has its own E02.
        let mut two_seasons = tests_support::run();
        let other = "/m/K-On!!/K-On!! - E02 - Clean-up.mkv";
        two_seasons.findings[0].file = other.into();
        let names = labels(&two_seasons);
        assert_eq!(names[other], "K-On!! - E02");
        assert_eq!(names[&tests_support::path(2)], "K-On! - E02");
        assert_eq!(names[&tests_support::path(3)], "K-On! - E03");
        // Seasons in folders of their own, with the same file names: the folder tells them apart.
        let mut folders = tests_support::run();
        folders.findings[0].file = "/m/Show/S1/Show - E01.mkv".into();
        folders.findings[1].file = "/m/Show/S2/Show - E01.mkv".into();
        let names = labels(&folders);
        assert_eq!(names["/m/Show/S1/Show - E01.mkv"], "S1/Show - E01");
        assert_eq!(names["/m/Show/S2/Show - E01.mkv"], "S2/Show - E01");
    }

    #[test]
    fn episodes_come_from_the_file_name() {
        assert_eq!(
            episode("/m/K-On!/K-On! - E01 - Disband the Club.mkv"),
            "E01"
        );
        assert_eq!(episode("/m/Show.S01E02.1080p.mkv"), "E02");
        assert_eq!(episode("/m/EVANGELION 1.11.mkv"), "EVANGELION 1");
        assert_eq!(episode("/m/Movie (2010).mkv"), "Movie (2010)");
        assert_eq!(
            long_episode("/m/K-On!!/K-On!! - E01 - Seniors.mkv"),
            "K-On!! - E01"
        );
        assert_eq!(long_episode("/m/Show.S01E02.1080p.mkv"), "Show.S01E02");
    }
}
