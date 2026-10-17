//! Everything subcheck keeps: run files of findings and decisions, and the scan cache.
//!
//! All of it lives in the state directory: `$XDG_STATE_HOME/subcheck/` when that is set, else
//! `~/.local/state/subcheck/` (`~/Library/Application Support/subcheck/` on macOS):
//! `runs/<run>.jsonl`, one append-only file per scan, and `cache.jsonl`.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunHeader {
    pub started: Timestamp,
    pub backend: String,
    pub command: Vec<String>,
    pub prompt_version: u32,
    pub version: String,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackRecord {
    /// mkvmerge track ID.
    pub id: usize,
    pub name: Option<String>,
    pub language: String,
    /// mpv's `--sid`: 1-based among the file's subtitle tracks of any codec.
    pub sid: usize,
    pub default: bool,
    /// `cues` or `walk`; for a track that could not be read, why.
    pub read: String,
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

/// A run file read back: its findings with the latest decision for each.
#[derive(Debug, Default)]
pub struct Run {
    pub name: String,
    pub header: Option<RunHeader>,
    pub files: Vec<FileRecord>,
    pub findings: Vec<Finding>,
    /// The latest decision per finding ID; a finding with none is pending.
    pub decisions: HashMap<u64, Decision>,
    /// Lines that did not parse (a run cut off mid-write leaves at most one).
    pub bad_lines: usize,
}

impl Run {
    pub fn state_of(&self, finding: u64) -> State {
        self.decisions
            .get(&finding)
            .map_or(State::Pending, |d| d.state)
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
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(&line) {
            Ok(Record::Run(h)) => run.header = Some(h),
            Ok(Record::File(f)) => run.files.push(f),
            Ok(Record::Finding(f)) => run.findings.push(f),
            Ok(Record::Decision(d)) => {
                run.decisions.insert(d.finding, d);
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
/// checked again: another LLM, other languages, or a new prompt.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckedWith {
    pub backend: String,
    /// Sorted, without repeats: `--lang eng --lang und` and `--lang und --lang eng` are one check.
    pub langs: Vec<String>,
    pub prompt_version: u32,
}

impl CheckedWith {
    pub fn new(backend: &str, langs: &[String], prompt_version: u32) -> CheckedWith {
        let mut langs = langs.to_vec();
        langs.sort();
        langs.dedup();
        CheckedWith {
            backend: backend.to_string(),
            langs,
            prompt_version,
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
    entries: HashMap<(String, CheckedWith), (u64, Timestamp)>,
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
                cache.entries.insert((e.path, e.with), (e.size, e.mtime));
            }
        }
        Ok(cache)
    }

    pub fn is_fresh(&self, path: &str, size: u64, mtime: Timestamp, with: &CheckedWith) -> bool {
        self.entries.get(&(path.to_string(), with.clone())) == Some(&(size, mtime))
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
    }
}
