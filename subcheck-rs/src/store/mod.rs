//! Everything subcheck keeps, in one SQLite database ([`Store`]): each scan, the checks it made of
//! each file, their findings, and the decisions on them. Findings and decisions belong to files
//! and checks; a run is the record of one scan and a view of its files.
//!
//! The database is `subcheck.db` in the state directory unless `SUBCHECK_DB` names another
//! ([`location`]). Other state, such as mpv's socket, stays in the state directory.
//!
//! The record types here are what the rest of subcheck works with: review's screen keeps a [`Run`]
//! in memory, assembled from the database, and a scan's events carry [`Record`]s to it.

mod db;
mod view;
mod write;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::llm::{Spent, Usage};
use crate::mkv::{self, Truncated};
use crate::text::Occurrence;

pub use db::{BUSY_TIMEOUT, Location, SCHEMA_VERSION, Store, location, location_from};
pub use view::Selector;
pub use write::{Attempt, Decided, Made, Saved, ScanRow, is_busy};

/// The state directory, from explicit inputs. An empty or relative `XDG_STATE_HOME` is ignored,
/// as the XDG spec says and as nudge-rs does: a relative one would resolve against the cwd.
///
/// An absolute `XDG_STATE_HOME` wins on macOS too: macOS has no such convention, so one that is
/// set was set on purpose (the tests rely on it to keep each run in a temporary directory).
pub fn state_dir_from(home: &Path, xdg_state: Option<&Path>, macos: bool) -> PathBuf {
    match xdg_state.filter(|p| p.is_absolute()) {
        Some(xdg) => xdg.join("subcheck"),
        None if macos => home.join("Library/Application Support/subcheck"),
        None => home.join(".local/state/subcheck"),
    }
}

/// `path` made absolute and lexically clean: `std::path::absolute`, then each `.` dropped and each
/// `..` popping the component before it (at the root it does nothing). A file's identity in the
/// store is its path as written, so `/m/x/../Show` must name the same files as `/m/Show`.
///
/// Lexical, not `canonicalize`, on purpose: stored paths are not symlink-resolved, so resolving
/// symlinks here would change every file's identity. The cost is that a `..` after a symlinked
/// directory goes to the link's parent, not to the target's.
pub fn absolute(path: &Path) -> io::Result<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in std::path::absolute(path)?.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

/// The state directory for this process.
pub fn state_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
        .ok_or_else(|| io::Error::other("HOME is not set to an absolute path"))?;
    let xdg = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    Ok(state_dir_from(
        &home,
        xdg.as_deref(),
        cfg!(target_os = "macos"),
    ))
}

/// What a scan saved, as its events carry it to review's model ([`Run::add`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    File(FileRecord),
    Finding(Finding),
    Decision(Decision),
    Usage(LlmCall),
}

/// What a scan was made with: a `scans` row.
#[derive(Debug, Clone, PartialEq)]
pub struct RunHeader {
    pub started: Timestamp,
    pub backend: String,
    pub command: Vec<String>,
    pub prompt_version: u32,
    pub version: String,
    /// The languages checked, as given (`scan --lang`).
    pub langs: Vec<String>,
}

/// One of the checks `scan` makes of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Check {
    /// Proofreading by the LLM.
    #[default]
    Llm,
    /// Tracks shifted by whole frames (`timing`).
    Timing,
}

/// A file as a view shows it, put together from its checks ([`FileRecord::merge`]).
#[derive(Debug, Clone, PartialEq)]
pub struct FileRecord {
    pub path: String,
    pub size: u64,
    pub mtime: Timestamp,
    /// `None` when the file was checked; the reason when it failed.
    pub error: Option<String>,
    pub tracks: Vec<TrackRecord>,
    /// Distinct lines checked.
    pub lines: usize,
    /// Set when the file is shorter than its Segment says: the findings cover only what is there.
    pub truncated: Option<Truncated>,
    /// Its current checks; none for a file that only ever failed.
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackRecord {
    /// mkvmerge track ID.
    pub id: usize,
    pub name: Option<String>,
    /// ISO 639-2, as the parser reads it: Matroska's default `eng` when the file gives none.
    pub language: String,
    /// LanguageBCP47, as the parser reads it. Left out of the JSON when absent, so a check from
    /// before it reads the same (and names a track tagged only in BCP 47 `(eng)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language_bcp47: Option<String>,
    /// mpv's `--sid`: 1-based among the file's subtitle tracks of any codec.
    pub sid: usize,
    pub default: bool,
    /// `cues` or `walk`; for a track that could not be read, why.
    pub read: String,
}

impl TrackRecord {
    /// The track's name in the reports, as [`crate::mkv::shown_name`] gives it.
    pub fn shown_name(&self) -> String {
        let language = mkv::shown_language(&self.language, self.language_bcp47.as_deref());
        mkv::shown_name(self.name.as_deref(), language)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// Given by the store: unique, never used again.
    pub id: u64,
    pub file: String,
    pub style: String,
    pub original: String,
    pub suggestion: String,
    pub reason: String,
    /// What produced it: `llm`.
    pub source: String,
    pub occurrences: Vec<Occurrence>,
    /// Up to two lines on either side, in time order, for context.
    pub before: Vec<String>,
    pub after: Vec<String>,
    /// A timing finding's fix (source `timing`).
    pub timing: Option<TimingFix>,
}

/// What makes a finding the same as another of its file, so that a decision on one carries to the
/// other ([`Finding::carry_key`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CarryKey {
    /// A typo: its line and fix.
    Text {
        path: String,
        original: String,
        suggestion: String,
    },
    /// A timing fix: its track, and how far it moves each stretch. What it rests on, the frame that
    /// shows it and the lines it names differ between timing rules; the fix `apply` makes does not.
    Timing {
        path: String,
        track: usize,
        frame_ns: u64,
        /// Each stretch's `from_ms`, `to_ms` and `frames`, in order.
        stretches: Vec<(Option<i64>, Option<i64>, i64)>,
    },
}

impl Finding {
    /// The check that made it, by its source.
    pub fn check(&self) -> Check {
        if self.source == "timing" {
            Check::Timing
        } else {
            Check::Llm
        }
    }

    /// Its [`CarryKey`].
    pub fn carry_key(&self) -> CarryKey {
        match &self.timing {
            Some(t) => CarryKey::Timing {
                path: self.file.clone(),
                track: t.track,
                frame_ns: t.frame_ns,
                stretches: t
                    .stretches
                    .iter()
                    .map(|s| (s.from_ms, s.to_ms, s.frames))
                    .collect(),
            },
            None => CarryKey::Text {
                path: self.file.clone(),
                original: self.original.clone(),
                suggestion: self.suggestion.clone(),
            },
        }
    }
}

/// A track shifted by whole frames, and how to put it back: every stretch moved by its frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingFix {
    /// mkvmerge track ID.
    pub track: usize,
    /// The video's frame duration.
    pub frame_ns: u64,
    /// One for a whole-track shift; a partial track's in time order, in-sync ones included.
    pub stretches: Vec<FixStretch>,
    /// Lines of a partial track left where they are: their times point both ways.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unshifted: Vec<LineRef>,
    pub basis: Basis,
}

/// Part of a track at one offset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixStretch {
    /// The first line's start; `None` from the start of the track.
    pub from_ms: Option<i64>,
    /// The last line's end; `None` to the end of the track.
    pub to_ms: Option<i64>,
    /// How late (positive) or early its lines are; 0 for a stretch in sync.
    pub frames: i64,
    /// The first and last lines it moves, as the scan read them; `None` in a stretch in sync or
    /// one with no lines of this track.
    pub first: Option<LineRef>,
    pub last: Option<LineRef>,
    /// The time of the frame that shows the error, for `o`: a cut's first frame for a late
    /// stretch, the frame before the cut for an early one.
    pub frame_ms: Option<i64>,
}

/// One event, by its raw text (all but its ReadOrder) and times.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRef {
    pub raw: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// What a timing finding rests on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Basis {
    /// The track's own times against the cuts.
    Cuts {
        times: usize,
        /// Windows at the offset (0 when the whole track decided it), of `windows`.
        agreeing: usize,
        windows: usize,
    },
    /// Its lines shared with another track of the file, `gap` frames after it.
    Lines {
        source: usize,
        /// Distinct shared times, and how many of them sit at `gap`.
        shared: usize,
        at_gap: usize,
        gap: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Pending,
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub finding: u64,
    pub state: State,
    /// The reviewer's own wording, when it replaces the suggestion.
    pub edited: Option<String>,
    pub at: Timestamp,
}

/// A finding's fix, made in its file by `apply` (or found there already).
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub finding: u64,
    pub at: Timestamp,
}

/// One LLM call a scan started with claude or codex, retries and failed ones too, with what its
/// backend says it used: a `usage` row. A backend that never says (ollama, `--llm-cmd`), or a call
/// that could not be started, leaves none.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmCall {
    /// The file whose lines it carried: a line other files share is sent once, with the first.
    pub file: String,
    pub lines: usize,
    /// `None`, and no numbers in the row, when its backend said nothing of it: killed at the
    /// timeout, failing with nothing on stdout, printing something other than its JSON, or JSON
    /// with no usage in it.
    pub usage: Option<Usage>,
}

impl FileRecord {
    /// Take a later record of the same file, as a view puts a file's checks together: a failed
    /// attempt marks the file failed and leaves its checks standing; a check adds its kind and
    /// the file as it read it, with the lines it read when it is the LLM's.
    pub fn merge(&mut self, later: FileRecord) {
        if later.error.is_some() {
            if self.checks.is_empty() {
                *self = later;
            } else {
                self.error = later.error;
            }
            return;
        }
        let lines = if later.checks.contains(&Check::Llm) || !self.checks.contains(&Check::Llm) {
            later.lines
        } else {
            self.lines
        };
        let mut checks = std::mem::take(&mut self.checks);
        checks.extend(later.checks.iter().copied());
        checks.sort();
        checks.dedup();
        *self = FileRecord {
            lines,
            checks,
            ..later
        };
    }
}

/// Some files as a view shows them ([`Store::view`]): a run's, or those under some paths; their
/// current findings with the latest decision for each. Review's screen keeps one in memory.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Run {
    pub name: String,
    pub header: Option<RunHeader>,
    pub files: Vec<FileRecord>,
    pub findings: Vec<Finding>,
    /// The latest decision per finding ID; a finding with none is pending.
    pub decisions: HashMap<u64, Decision>,
    /// Findings whose fix is in their file.
    pub applied: HashSet<u64>,
    /// What its LLM calls used, summed: none recorded in a run from before calls were.
    pub spent: Spent,
    /// The files under some paths, not a run's: `name` is the paths, quoted for the shell.
    pub of_paths: bool,
    /// For a paths view, the paths it shows, as given: review's screen names the view by them.
    pub paths: Vec<PathBuf>,
}

impl Run {
    /// Take one record as a view puts them together: a file's later record merges into its first,
    /// in its place ([`FileRecord::merge`]); the latest decision for a finding wins.
    pub fn add(&mut self, record: Record) {
        match record {
            Record::File(f) => match self.files.iter().position(|r| r.path == f.path) {
                Some(i) => self.files[i].merge(f),
                None => self.files.push(f),
            },
            Record::Finding(f) => self.findings.push(f),
            Record::Decision(d) => {
                self.decisions.insert(d.finding, d);
            }
            Record::Usage(call) => self.spent.add(call.usage.as_ref()),
        }
    }

    /// How messages name it: `run 2026-10-17T14-03-22`, or the paths it shows the files under.
    pub fn called(&self) -> String {
        if self.of_paths {
            self.name.clone()
        } else {
            format!("run {}", self.name)
        }
    }

    /// What review's screen calls the view, on the list pane's top border: `run <name>`; for a
    /// paths view, the last part of its first path, with `+N` for the others.
    pub fn shown_name(&self) -> String {
        if !self.of_paths {
            return format!("run {}", self.name);
        }
        let Some(first) = self.paths.first() else {
            return self.name.clone();
        };
        let name = first
            .file_name()
            .map_or_else(|| first.to_string_lossy(), |n| n.to_string_lossy())
            .into_owned();
        match self.paths.len() {
            1 => name,
            n => format!("{name} +{}", n - 1),
        }
    }

    pub fn state_of(&self, finding: u64) -> State {
        self.decisions
            .get(&finding)
            .map_or(State::Pending, |d| d.state)
    }
}

/// What a file was checked with. A change in any of it is a different check, so the file is
/// checked again: another check, LLM or set of languages, or a new prompt or timing rules.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct CheckedWith {
    pub check: Check,
    /// The LLM; empty for the timing check.
    pub backend: String,
    /// Sorted, without repeats: `--lang eng --lang und` and `--lang und --lang eng` are one check.
    pub langs: Vec<String>,
    /// The LLM's prompt version, or the timing rules' version.
    pub prompt_version: u32,
}

impl CheckedWith {
    /// The LLM check.
    pub fn new(backend: &str, langs: &[String], prompt_version: u32) -> CheckedWith {
        let mut langs = langs.to_vec();
        langs.sort();
        langs.dedup();
        CheckedWith {
            check: Check::Llm,
            backend: backend.to_string(),
            langs,
            prompt_version,
        }
    }

    /// The timing check, by the rules of `version`.
    pub fn timing(langs: &[String], version: u32) -> CheckedWith {
        CheckedWith {
            check: Check::Timing,
            ..CheckedWith::new("", langs, version)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_removes_dot_and_dotdot_by_the_text_alone() {
        let abs = |p: &str| absolute(Path::new(p)).unwrap();
        assert_eq!(abs("/m/x/../Show"), Path::new("/m/Show"));
        assert_eq!(abs("/m/Show/."), Path::new("/m/Show"));
        assert_eq!(abs("/m/./Show/./E01.mkv"), Path::new("/m/Show/E01.mkv"));
        assert_eq!(abs("/m/a/b/../../Show"), Path::new("/m/Show"));
        // At the root a `..` has nowhere to go.
        assert_eq!(abs("/.."), Path::new("/"));
        assert_eq!(abs("/../../m"), Path::new("/m"));
        assert_eq!(abs("/m/../.."), Path::new("/"));
        // A trailing slash is not a component.
        assert_eq!(abs("/m/Show/"), Path::new("/m/Show"));
        assert_eq!(abs("/m/x/../Show/"), Path::new("/m/Show"));
        // A relative path starts at the working directory.
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(abs("Show/./E01.mkv"), cwd.join("Show/E01.mkv"));
        assert_eq!(abs("x/../Show"), cwd.join("Show"));
        assert_eq!(abs("."), cwd);
    }

    #[test]
    fn state_dir_prefers_an_absolute_xdg_value() {
        let home = Path::new("/home/u");
        assert_eq!(
            state_dir_from(home, Some(Path::new("/x/state")), false),
            Path::new("/x/state/subcheck")
        );
        assert_eq!(
            state_dir_from(home, None, false),
            Path::new("/home/u/.local/state/subcheck")
        );
        assert_eq!(
            state_dir_from(home, Some(Path::new("relative")), false),
            Path::new("/home/u/.local/state/subcheck")
        );
        assert_eq!(
            state_dir_from(home, Some(Path::new("")), false),
            Path::new("/home/u/.local/state/subcheck")
        );
        assert_eq!(
            state_dir_from(home, None, true),
            Path::new("/home/u/Library/Application Support/subcheck")
        );
        // On macOS too, an XDG_STATE_HOME that is set wins: whoever set it meant it (the tests
        // point it at a temporary directory, and a macOS CI runner was ignoring that).
        assert_eq!(
            state_dir_from(home, Some(Path::new("/x/state")), true),
            Path::new("/x/state/subcheck")
        );
        assert_eq!(
            state_dir_from(home, Some(Path::new("relative")), true),
            Path::new("/home/u/Library/Application Support/subcheck")
        );
    }

    #[test]
    fn a_timing_fix_is_kept_as_json_of_its_own_shape() {
        // The `timing` column: what a timing finding's fix was, read back whole.
        let line = |start_ms| LineRef {
            raw: "0,D,,0,0,0,,Line.".into(),
            start_ms,
            end_ms: start_ms + 1000,
        };
        let fix = TimingFix {
            track: 5,
            frame_ns: 41_708_333,
            stretches: vec![FixStretch {
                from_ms: None,
                to_ms: None,
                frames: 1,
                first: Some(line(1000)),
                last: Some(line(9000)),
                frame_ms: Some(1_091_924),
            }],
            unshifted: vec![],
            basis: Basis::Cuts {
                times: 289,
                agreeing: 7,
                windows: 9,
            },
        };
        let json = serde_json::to_string(&fix).unwrap();
        assert!(
            json.contains(r#""basis":{"kind":"cuts","times":289,"agreeing":7,"windows":9}"#),
            "{json}"
        );
        assert!(!json.contains("unshifted"), "{json}");
        assert_eq!(serde_json::from_str::<TimingFix>(&json).unwrap(), fix);
    }

    #[test]
    fn a_files_later_records_merge_as_a_view_puts_its_checks_together() {
        let record = |checks: &[Check], lines, error: Option<&str>| FileRecord {
            path: "/m/a.mkv".into(),
            size: 1,
            mtime: Timestamp::UNIX_EPOCH,
            error: error.map(Into::into),
            tracks: vec![],
            lines,
            truncated: None,
            checks: checks.to_vec(),
        };
        // The timing check added alone keeps the LLM's lines.
        let mut f = record(&[Check::Llm], 412, None);
        f.merge(record(&[Check::Timing], 0, None));
        assert_eq!(f, record(&[Check::Llm, Check::Timing], 412, None));
        // A failed attempt leaves the checks standing.
        f.merge(record(&[], 0, Some("LLM: boom")));
        assert_eq!(
            f,
            record(&[Check::Llm, Check::Timing], 412, Some("LLM: boom"))
        );
        // Both made again: the new lines, and no failure.
        f.merge(record(&[Check::Llm, Check::Timing], 380, None));
        assert_eq!(f, record(&[Check::Llm, Check::Timing], 380, None));
        // A file that only ever failed is its failure, until a check is made.
        let mut failed = record(&[], 0, Some("cannot read"));
        failed.merge(record(&[], 0, Some("still cannot")));
        assert_eq!(failed, record(&[], 0, Some("still cannot")));
        failed.merge(record(&[Check::Timing], 0, None));
        assert_eq!(failed, record(&[Check::Timing], 0, None));
    }
}
