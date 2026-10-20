//! Everything subcheck keeps: run files of findings and decisions, and the scan cache.
//!
//! All of it lives in the state directory: `$XDG_STATE_HOME/subcheck/` when that is set, else
//! `~/.local/state/subcheck/` (`~/Library/Application Support/subcheck/` on macOS):
//! `runs/<run>.jsonl`, one append-only file per scan, and `cache.jsonl`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::mkv::{self, Truncated};
use crate::text::Occurrence;

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Run(RunHeader),
    File(FileRecord),
    Finding(Finding),
    Decision(Decision),
    Applied(Applied),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunHeader {
    pub started: Timestamp,
    pub backend: String,
    pub command: Vec<String>,
    pub prompt_version: u32,
    pub version: String,
    /// The languages checked, as given (`scan --lang`). Empty in runs from before it was kept.
    #[serde(default)]
    pub langs: Vec<String>,
}

/// One of the checks `scan` makes of a file.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    /// Proofreading by the LLM.
    #[default]
    Llm,
    /// Tracks shifted by whole frames (`timing`).
    Timing,
}

/// The checks of a file record from before they were kept: the LLM's only.
fn llm_only() -> Vec<Check> {
    vec![Check::Llm]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Left out of the JSON otherwise, so run files from before it read the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<Truncated>,
    /// The checks made of it. A later record for the same path replaces this one (a check added
    /// by `scan --run`, or a failed file checked again).
    #[serde(default = "llm_only")]
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackRecord {
    /// mkvmerge track ID.
    pub id: usize,
    pub name: Option<String>,
    /// ISO 639-2, as the parser reads it: Matroska's default `eng` when the file gives none.
    pub language: String,
    /// LanguageBCP47, as the parser reads it. Left out of the JSON when absent, so run files from
    /// before it read the same (and name a track tagged only in BCP 47 `(eng)`).
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// Unique within its run.
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
    /// A timing finding's fix (source `timing`). Left out of other findings' JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<TimingFix>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub finding: u64,
    pub state: State,
    /// The reviewer's own wording, when it replaces the suggestion.
    pub edited: Option<String>,
    pub at: Timestamp,
}

/// A finding's fix, made in its file by `apply` (or found there already).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub finding: u64,
    pub at: Timestamp,
}

/// An open run file that records are appended to.
pub struct RunWriter {
    pub name: String,
    pub path: PathBuf,
    file: File,
}

impl RunWriter {
    /// A new run file in `<state>/runs/`, named for the local time, `2026-10-17T14-03-22`, with
    /// `-2`, `-3`, ... when a run already started in the same second.
    pub fn create(state: &Path) -> io::Result<RunWriter> {
        let dir = state.join("runs");
        fs::create_dir_all(&dir)?;
        let base = jiff::Zoned::now().strftime("%Y-%m-%dT%H-%M-%S").to_string();
        for n in 1.. {
            let name = if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            let path = dir.join(format!("{name}.jsonl"));
            match OpenOptions::new().append(true).create_new(true).open(&path) {
                Ok(file) => return Ok(RunWriter { name, path, file }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        unreachable!("1.. never ends")
    }

    /// An existing run file, to append decisions to. A scan writing to it holds its lock and ends
    /// its own lines, so a line is ended here only when no one holds the lock.
    pub fn open(path: &Path) -> io::Result<RunWriter> {
        let mut w = RunWriter::open_as_is(path)?;
        match w.try_lock()? {
            Lock::Held => {
                w.end_last_line()?;
                w.unlock()?;
            }
            Lock::NoLocks => w.end_last_line()?,
            Lock::Busy => {}
        }
        Ok(w)
    }

    /// An existing run file, taken for this process alone (see [`RunWriter::lock`]) before
    /// anything is written to it: a scan adding to a run.
    pub fn open_locked(path: &Path) -> io::Result<RunWriter> {
        let mut w = RunWriter::open_as_is(path)?;
        w.lock()?;
        w.end_last_line()?;
        Ok(w)
    }

    fn open_as_is(path: &Path) -> io::Result<RunWriter> {
        let file = OpenOptions::new().read(true).append(true).open(path)?;
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(RunWriter {
            name,
            path: path.to_path_buf(),
            file,
        })
    }

    /// A scan cut off mid-write leaves a last line with no newline. End it, or the first record
    /// appended here would join that broken line and be lost with it.
    fn end_last_line(&mut self) -> io::Result<()> {
        if self.file.metadata()?.len() > 0 {
            let mut last = [0u8];
            self.file.seek(SeekFrom::End(-1))?;
            self.file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                self.file.write_all(b"\n")?;
            }
        }
        Ok(())
    }

    /// Take the run for this process alone (an advisory `flock`, let go when the file closes), so
    /// two scans never number findings in one run at once. Review and apply append without it:
    /// they number nothing.
    pub fn lock(&self) -> io::Result<()> {
        match self.try_lock()? {
            Lock::Held | Lock::NoLocks => Ok(()),
            Lock::Busy => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("run {} is being written to by another subcheck", self.name),
            )),
        }
    }

    /// The lock if no one else holds it. On a filesystem with no locks (some network mounts)
    /// there is none to take, and that is no error.
    fn try_lock(&self) -> io::Result<Lock> {
        match self.file.try_lock() {
            Ok(()) => Ok(Lock::Held),
            Err(std::fs::TryLockError::WouldBlock) => Ok(Lock::Busy),
            // By errno: EOPNOTSUPP and ENOTSUP are one on Linux, two on macOS. (Not by
            // `Unsupported`, which is also std's for a platform with no flock at all: that should
            // fail, not go on unlocked.)
            Err(std::fs::TryLockError::Error(e)) => match e.raw_os_error() {
                Some(code) if [libc::ENOLCK, libc::EOPNOTSUPP, libc::ENOTSUP].contains(&code) => {
                    Ok(Lock::NoLocks)
                }
                _ => Err(e),
            },
        }
    }

    fn unlock(&self) -> io::Result<()> {
        self.file.unlock()
    }

    /// Append records as JSON lines and flush them, so a crash loses nothing already written.
    pub fn append(&mut self, records: &[Record]) -> io::Result<()> {
        let mut buf = Vec::new();
        for r in records {
            serde_json::to_writer(&mut buf, r)?;
            buf.push(b'\n');
        }
        self.file.write_all(&buf)?;
        self.file.flush()
    }
}

/// What asking for a run file's lock got.
enum Lock {
    Held,
    /// Another subcheck holds it.
    Busy,
    /// The filesystem has no locks.
    NoLocks,
}

/// A run file read back: its findings with the latest decision for each.
#[derive(Debug, Default)]
pub struct Run {
    pub name: String,
    pub header: Option<RunHeader>,
    pub files: Vec<FileRecord>,
    pub findings: Vec<Finding>,
    /// The latest decision per finding ID; a finding with none is pending.
    pub decisions: HashMap<u64, Decision>,
    /// Findings whose fix is in their file.
    pub applied: HashSet<u64>,
    /// Lines that did not parse (a run cut off mid-write leaves at most one).
    pub bad_lines: usize,
}

impl Run {
    pub fn state_of(&self, finding: u64) -> State {
        self.decisions
            .get(&finding)
            .map_or(State::Pending, |d| d.state)
    }

    /// The ID for the next finding appended: one past the highest so far.
    pub fn next_finding_id(&self) -> u64 {
        self.findings
            .iter()
            .map(|f| f.id)
            .max()
            .map_or(1, |n| n + 1)
    }
}

pub fn read_run(path: &Path) -> io::Result<Run> {
    let mut run = Run {
        name: path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        ..Run::default()
    };
    // Where each path's file record is in `run.files`.
    let mut at: HashMap<String, usize> = HashMap::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(&line) {
            Ok(Record::Run(h)) => run.header = Some(h),
            // The latest record for a path wins, in the place of the first.
            Ok(Record::File(f)) => {
                if let Some(&i) = at.get(&f.path) {
                    run.files[i] = f;
                } else {
                    at.insert(f.path.clone(), run.files.len());
                    run.files.push(f);
                }
            }
            Ok(Record::Finding(f)) => run.findings.push(f),
            Ok(Record::Decision(d)) => {
                run.decisions.insert(d.finding, d);
            }
            Ok(Record::Applied(a)) => {
                run.applied.insert(a.finding);
            }
            Err(_) => run.bad_lines += 1,
        }
    }
    Ok(run)
}

/// The run file named `name` (with or without `.jsonl`, or a path), or the newest run.
pub fn find_run(state: &Path, name: Option<&str>) -> io::Result<PathBuf> {
    let dir = state.join("runs");
    if let Some(name) = name {
        let as_path = Path::new(name);
        if as_path.is_file() {
            return Ok(as_path.to_path_buf());
        }
        let file = dir.join(format!("{}.jsonl", name.trim_end_matches(".jsonl")));
        return if file.is_file() {
            Ok(file)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no run {name} in {}", dir.display()),
            ))
        };
    }
    let mut runs: Vec<PathBuf> = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    runs.sort_by_key(|p| run_order(p));
    runs.pop().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no runs yet in {}", dir.display()),
        )
    })
}

/// Sort key for run files: the start time, then the `-N` of a run that started in the same second.
/// A plain sort gets that wrong: `-` sorts before `.`, so `...22-2.jsonl` would come before
/// `...22.jsonl`.
fn run_order(path: &Path) -> (String, u32) {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    // A name is `2026-10-17T14-03-22`, 19 characters, then an optional `-N`.
    let suffix = stem
        .get(19..)
        .and_then(|s| s.strip_prefix('-'))
        .and_then(|n| n.parse::<u32>().ok());
    match (suffix, stem.get(..19)) {
        (Some(n), Some(base)) => (base.to_string(), n),
        _ => (stem, 1),
    }
}

/// What a file was checked with. A change in any of it is a different check, so the file is
/// checked again: another check, LLM or set of languages, or a new prompt or timing rules.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckedWith {
    /// An entry from before checks were kept is the LLM's.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub path: String,
    pub size: u64,
    pub mtime: Timestamp,
    /// Flattened, so an entry is one flat JSON object; an entry from before a field existed reads
    /// it as empty or 0 and simply never matches, which checks that file once more.
    #[serde(flatten)]
    pub with: CheckedWith,
    pub run: String,
}

/// Files already checked: a scan skips one whose size and mtime still match an entry made with the
/// same [`CheckedWith`].
#[derive(Debug, Default)]
pub struct Cache {
    /// The size, mtime and run of each file checked, by path and what checked it.
    entries: HashMap<(String, CheckedWith), (u64, Timestamp, String)>,
}

impl Cache {
    pub fn load(state: &Path) -> io::Result<Cache> {
        let mut cache = Cache::default();
        let file = match File::open(state.join("cache.jsonl")) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(cache),
            Err(e) => return Err(e),
        };
        for line in BufReader::new(file).lines() {
            if let Ok(e) = serde_json::from_str::<CacheEntry>(&line?) {
                cache
                    .entries
                    .insert((e.path, e.with), (e.size, e.mtime, e.run));
            }
        }
        Ok(cache)
    }

    pub fn is_fresh(&self, path: &str, size: u64, mtime: Timestamp, with: &CheckedWith) -> bool {
        self.run_of(path, size, mtime, with).is_some()
    }

    /// The languages run `run`'s LLM checks were made with, as the cache recorded them: a run from
    /// before runs kept their languages says so nowhere else.
    pub fn langs_of(&self, run: &str) -> Option<Vec<String>> {
        self.entries
            .iter()
            .find(|((_, with), (_, _, r))| r == run && with.check == Check::Llm)
            .map(|((_, with), _)| with.langs.clone())
    }

    /// The run that checked the file as `with` says, if it is unchanged since.
    pub fn run_of(
        &self,
        path: &str,
        size: u64,
        mtime: Timestamp,
        with: &CheckedWith,
    ) -> Option<&str> {
        let (s, m, run) = self.entries.get(&(path.to_string(), with.clone()))?;
        (*s == size && *m == mtime).then_some(run.as_str())
    }

    pub fn append(state: &Path, entry: &CacheEntry) -> io::Result<()> {
        fs::create_dir_all(state)?;
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(state.join("cache.jsonl"))?
            .write_all(&line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn finding(id: u64) -> Finding {
        Finding {
            id,
            file: "/m/a.mkv".into(),
            style: "D".into(),
            original: "befor".into(),
            suggestion: "before".into(),
            reason: "typo".into(),
            source: "llm".into(),
            occurrences: vec![Occurrence {
                track: 2,
                start_ms: 1000,
                end_ms: 2000,
            }],
            before: vec![],
            after: vec!["next".into()],
            timing: None,
        }
    }

    #[test]
    fn a_run_reads_back_with_the_latest_decision_winning() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = RunWriter::create(dir.path()).unwrap();
        let at: Timestamp = "2026-10-17T10:00:00Z".parse().unwrap();
        w.append(&[Record::Finding(finding(1)), Record::Finding(finding(2))])
            .unwrap();
        w.append(&[
            Record::Decision(Decision {
                finding: 1,
                state: State::Accepted,
                edited: None,
                at,
            }),
            Record::Decision(Decision {
                finding: 1,
                state: State::Rejected,
                edited: None,
                at,
            }),
        ])
        .unwrap();
        let run = read_run(&w.path).unwrap();
        assert_eq!(run.findings.len(), 2);
        assert_eq!(run.state_of(1), State::Rejected);
        assert_eq!(run.state_of(2), State::Pending);
        assert_eq!(run.name, w.name);
    }

    #[test]
    fn a_file_record_says_truncated_only_when_it_was() {
        let file = |truncated| FileRecord {
            path: "/m/a.mkv".into(),
            size: 1,
            mtime: Timestamp::UNIX_EPOCH,
            error: None,
            tracks: vec![],
            lines: 0,
            truncated,
            checks: vec![Check::Llm],
        };
        // Whole files are written as before, so run files from before the field read the same.
        let whole = serde_json::to_string(&Record::File(file(None))).unwrap();
        assert!(!whole.contains("truncated"), "{whole}");
        assert_eq!(
            serde_json::from_str::<Record>(&whole).unwrap(),
            Record::File(file(None))
        );
        let cut = Record::File(file(Some(Truncated {
            declared_end: 2,
            file_size: 1,
        })));
        let line = serde_json::to_string(&cut).unwrap();
        assert!(
            line.contains(r#""truncated":{"declared_end":2,"file_size":1}"#),
            "{line}"
        );
        assert_eq!(serde_json::from_str::<Record>(&line).unwrap(), cut);
    }

    #[test]
    fn a_timing_finding_round_trips_and_other_findings_are_written_as_before() {
        let line = |start_ms| LineRef {
            raw: "0,D,,0,0,0,,Line.".into(),
            start_ms,
            end_ms: start_ms + 1000,
        };
        let timing = Finding {
            style: String::new(),
            original: "1 frame late".into(),
            suggestion: "shift 1 frame earlier".into(),
            reason: "cuts: 289 times, +1 in 7 of 9 windows".into(),
            source: "timing".into(),
            after: vec![],
            timing: Some(TimingFix {
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
            }),
            ..finding(1)
        };
        let json = serde_json::to_string(&Record::Finding(timing.clone())).unwrap();
        assert!(
            json.contains(r#""basis":{"kind":"cuts","times":289,"agreeing":7,"windows":9}"#),
            "{json}"
        );
        assert!(!json.contains("unshifted"), "{json}");
        assert_eq!(
            serde_json::from_str::<Record>(&json).unwrap(),
            Record::Finding(timing)
        );
        let plain = serde_json::to_string(&Record::Finding(finding(2))).unwrap();
        assert!(!plain.contains("timing"), "{plain}");
    }

    #[test]
    fn records_from_before_checks_were_kept_read_as_the_llms() {
        let file = r#"{"type":"file","path":"/m/a.mkv","size":1,"mtime":"1970-01-01T00:00:00Z","error":null,"tracks":[],"lines":0}"#;
        let Ok(Record::File(f)) = serde_json::from_str::<Record>(file) else {
            panic!("{file}")
        };
        assert_eq!(f.checks, [Check::Llm]);
        let header = r#"{"type":"run","started":"1970-01-01T00:00:00Z","backend":"claude","command":[],"prompt_version":1,"version":"0.1.0"}"#;
        let Ok(Record::Run(h)) = serde_json::from_str::<Record>(header) else {
            panic!("{header}")
        };
        assert_eq!(h.langs, Vec::<String>::new());
        let entry = r#"{"path":"/m/a.mkv","size":1,"mtime":"1970-01-01T00:00:00Z","backend":"claude","langs":["eng"],"prompt_version":1,"run":"r"}"#;
        let e: CacheEntry = serde_json::from_str(entry).unwrap();
        assert_eq!(e.with, CheckedWith::new("claude", &["eng".into()], 1));
    }

    #[test]
    fn the_latest_file_record_for_a_path_wins_and_ids_continue_past_the_highest() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = RunWriter::create(dir.path()).unwrap();
        let file = |path: &str, checks: Vec<Check>| FileRecord {
            path: path.into(),
            size: 1,
            mtime: Timestamp::UNIX_EPOCH,
            error: None,
            tracks: vec![],
            lines: 0,
            truncated: None,
            checks,
        };
        w.append(&[
            Record::File(file("/m/a.mkv", vec![Check::Llm])),
            Record::File(file("/m/b.mkv", vec![Check::Llm])),
            Record::Finding(finding(7)),
            Record::Finding(finding(3)),
            Record::File(file("/m/a.mkv", vec![Check::Llm, Check::Timing])),
        ])
        .unwrap();
        let run = read_run(&w.path).unwrap();
        let files: Vec<(&str, &[Check])> = run
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.checks.as_slice()))
            .collect();
        assert_eq!(
            files,
            [
                ("/m/a.mkv", [Check::Llm, Check::Timing].as_slice()),
                ("/m/b.mkv", [Check::Llm].as_slice()),
            ]
        );
        assert_eq!(run.next_finding_id(), 8);
        assert_eq!(Run::default().next_finding_id(), 1);
    }

    #[test]
    fn decisions_appended_to_an_existing_run_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = RunWriter::create(dir.path()).unwrap();
        w.append(&[Record::Finding(finding(1))]).unwrap();
        let mut again = RunWriter::open(&w.path).unwrap();
        assert_eq!(again.name, w.name);
        again
            .append(&[Record::Decision(Decision {
                finding: 1,
                state: State::Accepted,
                edited: Some("Mine".into()),
                at: Timestamp::UNIX_EPOCH,
            })])
            .unwrap();
        let run = read_run(&w.path).unwrap();
        assert_eq!(run.findings.len(), 1);
        assert_eq!(run.state_of(1), State::Accepted);
        assert!(RunWriter::open(&dir.path().join("runs/none.jsonl")).is_err());
    }

    #[test]
    fn applied_records_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = RunWriter::create(dir.path()).unwrap();
        w.append(&[
            Record::Finding(finding(1)),
            Record::Finding(finding(2)),
            Record::Applied(Applied {
                finding: 2,
                at: Timestamp::UNIX_EPOCH,
            }),
        ])
        .unwrap();
        let text = std::fs::read_to_string(&w.path).unwrap();
        assert!(
            text.ends_with(
                "{\"type\":\"applied\",\"finding\":2,\"at\":\"1970-01-01T00:00:00Z\"}\n"
            ),
            "{text}"
        );
        assert_eq!(read_run(&w.path).unwrap().applied, [2].into());
    }

    #[test]
    fn a_decision_after_a_line_cut_off_mid_write_is_kept() {
        // A scan killed mid-write leaves its last line unfinished. The first decision appended
        // after it must start a line of its own, or it is lost with the broken one.
        let dir = tempfile::tempdir().unwrap();
        let mut w = RunWriter::create(dir.path()).unwrap();
        w.append(&[Record::Finding(finding(1))]).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&w.path)
            .unwrap()
            .write_all(b"{\"type\":\"fin")
            .unwrap();
        RunWriter::open(&w.path)
            .unwrap()
            .append(&[Record::Decision(Decision {
                finding: 1,
                state: State::Accepted,
                edited: None,
                at: Timestamp::UNIX_EPOCH,
            })])
            .unwrap();
        let run = read_run(&w.path).unwrap();
        assert_eq!((run.state_of(1), run.bad_lines), (State::Accepted, 1));
    }

    #[test]
    fn a_line_cut_off_mid_write_is_counted_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = RunWriter::create(dir.path()).unwrap();
        w.append(&[Record::Finding(finding(1))]).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&w.path)
            .unwrap()
            .write_all(b"{\"type\":\"fin")
            .unwrap();
        let run = read_run(&w.path).unwrap();
        assert_eq!((run.findings.len(), run.bad_lines), (1, 1));
    }

    #[test]
    fn a_run_locked_by_one_writer_is_refused_to_another() {
        let dir = tempfile::tempdir().unwrap();
        let first = RunWriter::create(dir.path()).unwrap();
        first.lock().unwrap();
        let second = RunWriter::open(&first.path).unwrap();
        let e = second.lock().unwrap_err();
        assert_eq!(
            e.to_string(),
            format!("run {} is being written to by another subcheck", first.name)
        );
        drop(first);
        second.lock().unwrap();
    }

    #[test]
    fn a_locked_run_is_left_as_it_is_by_another_writer() {
        let dir = tempfile::tempdir().unwrap();
        let first = RunWriter::create(dir.path()).unwrap();
        first.lock().unwrap();
        // The first scan is part way through a record.
        OpenOptions::new()
            .append(true)
            .open(&first.path)
            .unwrap()
            .write_all(b"{\"type\":\"fin")
            .unwrap();
        assert!(RunWriter::open_locked(&first.path).is_err());
        // Nor does review or apply opening it.
        RunWriter::open(&first.path).unwrap();
        // Nothing was written: no newline in the middle of the record.
        assert_eq!(fs::read(&first.path).unwrap(), b"{\"type\":\"fin");
    }

    #[test]
    fn runs_started_in_the_same_second_get_distinct_names() {
        let dir = tempfile::tempdir().unwrap();
        let a = RunWriter::create(dir.path()).unwrap();
        let b = RunWriter::create(dir.path()).unwrap();
        assert_ne!(a.path, b.path);
    }

    #[test]
    fn find_run_takes_a_name_or_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_run(dir.path(), None).is_err());
        let runs = dir.path().join("runs");
        fs::create_dir_all(&runs).unwrap();
        for name in [
            "2026-10-17T09-00-00",
            "2026-10-17T10-00-00",
            "2026-10-16T23-00-00",
        ] {
            fs::write(runs.join(format!("{name}.jsonl")), "").unwrap();
        }
        assert_eq!(
            find_run(dir.path(), None).unwrap(),
            runs.join("2026-10-17T10-00-00.jsonl")
        );
        assert_eq!(
            find_run(dir.path(), Some("2026-10-17T09-00-00")).unwrap(),
            runs.join("2026-10-17T09-00-00.jsonl")
        );
        assert!(find_run(dir.path(), Some("nope")).is_err());
    }

    #[test]
    fn a_same_second_run_counts_as_newer_by_its_number() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        fs::create_dir_all(&runs).unwrap();
        for name in [
            "2026-10-17T10-00-00",
            "2026-10-17T10-00-00-2",
            "2026-10-17T10-00-00-10",
            "2026-10-17T10-00-00-9",
        ] {
            fs::write(runs.join(format!("{name}.jsonl")), "").unwrap();
        }
        assert_eq!(
            find_run(dir.path(), None).unwrap(),
            runs.join("2026-10-17T10-00-00-10.jsonl")
        );
    }

    #[test]
    fn cache_matches_on_file_and_on_what_checked_it() {
        let dir = tempfile::tempdir().unwrap();
        let t: Timestamp = "2026-10-17T10:00:00Z".parse().unwrap();
        let with = CheckedWith::new("claude", &["eng".into()], 1);
        let entry = CacheEntry {
            path: "/m/a.mkv".into(),
            size: 10,
            mtime: t,
            with: with.clone(),
            run: "r".into(),
        };
        Cache::append(dir.path(), &entry).unwrap();
        let cache = Cache::load(dir.path()).unwrap();
        assert!(cache.is_fresh("/m/a.mkv", 10, t, &with));
        assert_eq!(cache.run_of("/m/a.mkv", 10, t, &with), Some("r"));
        assert!(!cache.is_fresh("/m/a.mkv", 11, t, &with));
        let later: Timestamp = "2026-10-17T10:00:01Z".parse().unwrap();
        assert!(!cache.is_fresh("/m/a.mkv", 10, later, &with));
        assert!(!cache.is_fresh("/m/b.mkv", 10, t, &with));
        // Another backend, other languages, or a new prompt is a different check.
        let gpt = CheckedWith::new("gpt", &["eng".into()], 1);
        let und = CheckedWith::new("claude", &["und".into()], 1);
        let v2 = CheckedWith::new("claude", &["eng".into()], 2);
        assert!(!cache.is_fresh("/m/a.mkv", 10, t, &gpt));
        assert!(!cache.is_fresh("/m/a.mkv", 10, t, &und));
        assert!(!cache.is_fresh("/m/a.mkv", 10, t, &v2));
        // Languages are a set: order and repeats do not matter.
        let same = CheckedWith::new("claude", &["eng".into(), "eng".into()], 1);
        assert!(cache.is_fresh("/m/a.mkv", 10, t, &same));
        // The timing check of the same file is a check of its own.
        assert!(!cache.is_fresh("/m/a.mkv", 10, t, &CheckedWith::timing(&["eng".into()], 1)));
    }
}
