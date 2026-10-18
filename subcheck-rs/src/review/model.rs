//! Review state: a run's findings grouped into rows, what the list shows, and what is under way.

use std::collections::hash_map::Entry as Slot;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

use jiff::Timestamp;
use ratatui::layout::Rect;

use super::layout;
use crate::list::{self, Filter};
use crate::store::{Decision, Finding, Run, State};

/// Findings with the same original and suggestion: one row, decided together. Repeats are common:
/// an OP lyric is a finding in every episode.
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
    pub expanded: HashSet<usize>,
    pub filter: Filter,
    /// Index into `entries()`.
    pub selected: usize,
    pub editor: Option<Editor>,
    /// Decisions made in this session, oldest first.
    pub undo: Vec<UndoStep>,
    /// Files that are not as they were scanned, by path.
    pub notes: HashMap<String, FileNote>,
    pub status: Option<String>,
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
    pub fn new(run: Run, notes: HashMap<String, FileNote>) -> Model {
        let rows = group(&run);
        let labels = labels(&run);
        Model {
            run,
            rows,
            labels,
            expanded: HashSet::new(),
            filter: Filter::Only(State::Pending),
            selected: 0,
            editor: None,
            undo: Vec::new(),
            notes,
            status: None,
            should_quit: false,
            area: Rect::default(),
            scroll: 0,
            detail_scroll: 0,
            last_click: None,
        }
    }

    /// The list's scroll offset on a screen of `area`: `scroll`, moved just enough to show the
    /// selection. The view draws with it and `update` keeps it.
    pub fn list_scroll(&self, area: Rect) -> usize {
        let rows = layout::list_rows(&layout::panes(area)).height;
        let len = self.entries().len();
        layout::keep_in_view(self.scroll, self.selected, usize::from(rows), len)
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
}

/// The run's findings as rows: identical original and suggestion share one, which sits where its
/// first member does.
pub fn group(run: &Run) -> Vec<Row> {
    let mut order: Vec<usize> = (0..run.findings.len()).collect();
    order.sort_by_cached_key(|&i| {
        let f = &run.findings[i];
        let start = list::shown_at(run, f).0.map_or(0, |o| o.start_ms);
        (f.file.clone(), start, f.id)
    });
    let mut rows: Vec<Row> = Vec::new();
    let mut by_text: HashMap<(&str, &str), usize> = HashMap::new();
    for i in order {
        let f = &run.findings[i];
        match by_text.entry((f.original.as_str(), f.suggestion.as_str())) {
            Slot::Occupied(slot) => rows[*slot.get()].members.push(i),
            Slot::Vacant(slot) => {
                slot.insert(rows.len());
                rows.push(Row { members: vec![i] });
            }
        }
    }
    rows
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

/// A small run for the review's tests: the OP lyric "trough" in E01, E02 and E03, and one other
/// finding in E02. Four findings, two rows.
#[cfg(test)]
pub mod tests_support {
    use super::{Model, Row};
    use crate::store::{FileRecord, Finding, Run, TrackRecord};
    use crate::text::Occurrence;

    pub fn path(ep: u32) -> String {
        format!("/m/K-On!/K-On! - E{ep:02} - Episode.mkv")
    }

    pub fn file(ep: u32) -> FileRecord {
        let track = |id, sid, name: &str, default| TrackRecord {
            id,
            name: Some(name.into()),
            language: "eng".into(),
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
