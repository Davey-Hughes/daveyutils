//! `subcheck scan`: find MKVs, read their text subtitle tracks, send the lines to the LLM in
//! chunks, and record every suggested fix in a run file.
//!
//! One reader thread reads files in order (the array's disks are the bottleneck, so never two at
//! once), `-j` workers run the LLM, and the calling thread collects: it appends a file's records
//! the moment every one of that file's lines has an answer.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use jiff::Timestamp;

use crate::blocks::{self, ReadPath};
use crate::llm::{self, Backend, Suggestion};
use crate::mkv::{self, Layout, TRACK_TYPE_SUBTITLE, Track, Truncated};
use crate::store::{
    self, Cache, CacheEntry, Check, CheckedWith, FileRecord, Finding, Record, Run, RunHeader,
    RunWriter, TrackRecord,
};
use crate::subs::Kind;
use crate::text::{self, Event, Line};
use crate::timing::{self, record, shared};

/// The subtitle languages a scan checks unless told otherwise.
pub const DEFAULT_LANG: &str = "eng";

pub struct Options {
    pub backend: Backend,
    pub chunk_lines: usize,
    pub jobs: usize,
    /// ISO 639-2 codes; a track matches by `Language` or by its BCP 47 primary subtag.
    pub langs: Vec<String>,
    pub force: bool,
    pub verbose: bool,
    /// The run file to add to (`--run`) instead of starting a run.
    pub into: Option<PathBuf>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub run: String,
    pub files: usize,
    pub checked: usize,
    pub failed: usize,
    pub cached: usize,
    pub findings: usize,
    /// Of the findings, those of the timing check.
    pub timing: usize,
    /// With `--run`: files left out as changed since the run.
    pub changed: usize,
    /// With `--run`: files whose LLM check is in another run, and was not made again.
    pub elsewhere: usize,
}

/// `paths` expanded to MKV files: files as given, directories walked recursively for `*.mkv`
/// (any case), skipping hidden entries (`.Recycle.Bin`, `.Trash-1000`, ...). Sorted, absolute.
/// Also returns a warning for each directory or entry that could not be read: it is skipped, so
/// one locked directory cannot stop a scan of a whole share.
pub fn mkv_files(paths: &[PathBuf]) -> io::Result<(Vec<PathBuf>, Vec<String>)> {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for p in paths {
        let p = std::path::absolute(p)?;
        if p.is_dir() {
            walk(&p, &mut out, &mut warnings);
        } else if p.is_file() {
            out.push(p);
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: no such file or directory", p.display()),
            ));
        }
    }
    out.sort();
    out.dedup();
    Ok((out, warnings))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, warnings: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            warnings.push(format!("{}: skipped: {e}", dir.display()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                warnings.push(format!("{}: skipped an entry: {e}", dir.display()));
                continue;
            }
        };
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk(&path, out, warnings),
            Ok(_) => {
                if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("mkv"))
                {
                    out.push(path);
                }
            }
            Err(e) => warnings.push(format!("{}: skipped: {e}", path.display())),
        }
    }
}

/// ISO 639-2 code to the ISO 639-1 code BCP 47 uses, for the languages subtitles here come in.
fn iso639_1(code: &str) -> Option<&'static str> {
    Some(match code {
        "eng" => "en",
        "jpn" => "ja",
        "fre" | "fra" => "fr",
        "ger" | "deu" => "de",
        "spa" => "es",
        "ita" => "it",
        "por" => "pt",
        "rus" => "ru",
        "chi" | "zho" => "zh",
        "kor" => "ko",
        _ => return None,
    })
}

/// Whether a track is in one of `langs`. Matroska's rule: LanguageBCP47, when present, overrides
/// Language, and an absent Language means "eng", so a track tagged only `ja` in BCP 47 is not
/// English.
pub fn wanted_language(track: &Track, langs: &[String]) -> bool {
    langs
        .iter()
        .any(|want| match track.language_bcp47.as_deref() {
            Some(tag) => {
                let primary = tag.split('-').next().unwrap_or(tag);
                primary.eq_ignore_ascii_case(want)
                    || iso639_1(want).is_some_and(|code| primary.eq_ignore_ascii_case(code))
            }
            None => track.language == *want,
        })
}

/// A file read and turned into lines, or the reason it could not be.
struct Prepared {
    path: PathBuf,
    size: u64,
    mtime: Timestamp,
    /// The checks made of it.
    checks: Vec<Check>,
    tracks: Vec<TrackRecord>,
    /// For the LLM; empty when it does not check the file.
    lines: Vec<Line>,
    /// The timing check's findings, numbered 0, and its notes (always shown).
    timing: Vec<Finding>,
    timing_notes: Vec<String>,
    error: Option<String>,
    /// A track could not be read; the file is then not cached, so it is tried again.
    skipped_tracks: bool,
    /// The file is shorter than its Segment says. Not cached either, so every scan says so until
    /// the copy is replaced.
    truncated: Option<Truncated>,
    /// Shown with `-v`.
    notes: Vec<String>,
}

/// A file's size and modification time: what the cache and review compare.
pub fn file_stamp(path: &Path) -> io::Result<(u64, Timestamp)> {
    let meta = std::fs::metadata(path)?;
    let mtime = Timestamp::try_from(meta.modified()?).map_err(io::Error::other)?;
    Ok((meta.len(), mtime))
}

fn prepare(path: &Path, langs: &[String], checks: &[Check]) -> Prepared {
    let mut p = Prepared {
        path: path.to_path_buf(),
        size: 0,
        mtime: Timestamp::UNIX_EPOCH,
        checks: checks.to_vec(),
        tracks: Vec::new(),
        lines: Vec::new(),
        timing: Vec::new(),
        timing_notes: Vec::new(),
        error: None,
        skipped_tracks: false,
        truncated: None,
        notes: Vec::new(),
    };
    match file_stamp(path) {
        Ok((size, mtime)) => (p.size, p.mtime) = (size, mtime),
        Err(e) => {
            p.error = Some(e.to_string());
            return p;
        }
    }
    let (r, layout) = match mkv::open(path) {
        Ok(x) => x,
        Err(e) => {
            p.error = Some(e.to_string());
            return p;
        }
    };
    p.truncated = layout.truncated;
    // The wanted tracks, in file order, with mpv's sid for each.
    let mut wanted = Vec::new();
    let mut sid = 0;
    for track in &layout.tracks {
        if track.kind == TRACK_TYPE_SUBTITLE {
            sid += 1;
        }
        if let Some(kind) = Kind::of(track)
            && wanted_language(track, langs)
        {
            wanted.push((track, kind, sid));
        }
    }
    let numbers: Vec<u64> = wanted.iter().map(|(t, _, _)| t.number).collect();
    let before = r.bytes_read();
    let reads = blocks::read_each(&r, &layout, &numbers);
    let mut events: Vec<(usize, Vec<Event>)> = Vec::new();
    for ((track, kind, sid), read) in wanted.into_iter().zip(reads) {
        let named = format!("track {} {}", track.id, track.shown_name());
        let read = match read {
            Ok(t) => {
                let path = match t.path {
                    ReadPath::Cues => "cues",
                    ReadPath::Walk => "walk",
                    ReadPath::Empty => "empty",
                };
                p.notes
                    .push(format!("{named}: {} events via {path}", t.blocks.len()));
                let mine = text::events(
                    track.id,
                    kind,
                    &t.blocks,
                    layout.timestamp_scale,
                    track.default_duration,
                );
                events.push((track.id, mine));
                path.to_string()
            }
            // An unsupported encoding loses that track, not the file.
            Err(e) => {
                p.notes.push(format!("{named}: skipped: {e}"));
                p.skipped_tracks = true;
                format!("skipped: {e}")
            }
        };
        p.tracks.push(TrackRecord {
            id: track.id,
            name: track.name.clone(),
            language: track.language.clone(),
            language_bcp47: track.language_bcp47.clone(),
            sid,
            default: track.default,
            read,
        });
    }
    p.notes
        .push(format!("{} bytes read", r.bytes_read() - before));
    if checks.contains(&Check::Timing) && !events.is_empty() {
        match timing_findings(path, &layout, &events) {
            Ok((found, notes)) => (p.timing, p.timing_notes) = (found, notes),
            Err(reason) => p.notes.push(format!("timing: unsupported: {reason}")),
        }
    }
    if checks.contains(&Check::Llm) {
        p.lines = text::lines(events.into_iter().flat_map(|(_, e)| e).collect());
    }
    p
}

/// The timing check of one file's tracks (mkvmerge ID and events each): its findings, numbered
/// 0, and a note for each track its shared lines contradict. Err: why the video cannot stand in
/// for the cuts, which is no failure of the file.
fn timing_findings(
    path: &Path,
    layout: &Layout,
    tracks: &[(usize, Vec<Event>)],
) -> Result<(Vec<Finding>, Vec<String>), String> {
    let video = timing::video(layout)?;
    let measured: Vec<timing::Analysis> = tracks
        .iter()
        .map(|(_, events)| timing::analyse(&video, events))
        .collect();
    let analysed: Vec<(usize, &[Event], &timing::Analysis)> = tracks
        .iter()
        .zip(&measured)
        .map(|((id, events), a)| (*id, events.as_slice(), a))
        .collect();
    let said = shared::lines_of(&video, &analysed);
    let views: Vec<record::Track> = analysed
        .iter()
        .zip(&said)
        .map(|(&(id, events, analysis), lines)| record::Track {
            id,
            events,
            analysis,
            lines: lines.as_ref(),
        })
        .collect();
    Ok(record::findings(&path.to_string_lossy(), &video, &views))
}

/// Lines bound for the LLM.
pub struct Chunk {
    pub id: usize,
    pub lines: Vec<Line>,
}

/// Packs lines into chunks, sending each distinct text once per run: a text already queued for
/// an earlier file points at that chunk instead of joining a new one.
pub struct Chunker {
    size: usize,
    next_id: usize,
    queued: HashMap<String, (usize, usize)>,
}

impl Chunker {
    pub fn new(size: usize) -> Chunker {
        Chunker {
            size: size.max(1),
            next_id: 0,
            queued: HashMap::new(),
        }
    }

    /// For each line, the (chunk ID, index in chunk) that will answer it, and the new chunks to
    /// send. A file's new lines never share a chunk with another file's.
    pub fn assign(&mut self, lines: &[Line]) -> (Vec<(usize, usize)>, Vec<Chunk>) {
        let mut refs = Vec::with_capacity(lines.len());
        let mut chunks: Vec<Chunk> = Vec::new();
        for line in lines {
            if let Some(&r) = self.queued.get(&line.text) {
                refs.push(r);
                continue;
            }
            if chunks.last().is_none_or(|c| c.lines.len() >= self.size) {
                chunks.push(Chunk {
                    id: self.next_id,
                    lines: Vec::new(),
                });
                self.next_id += 1;
            }
            let chunk = chunks.last_mut().expect("pushed above");
            let r = (chunk.id, chunk.lines.len());
            chunk.lines.push(line.clone());
            self.queued.insert(line.text.clone(), r);
            refs.push(r);
        }
        (refs, chunks)
    }
}

/// "1 file", "3 files".
pub fn files(n: usize) -> String {
    if n == 1 {
        "1 file".into()
    } else {
        format!("{n} files")
    }
}

/// A file to read, and the checks it needs.
type Todo = (PathBuf, Vec<Check>);

/// Each file with the checks it needs, and how many files need none: unchanged since an earlier
/// scan made every check, the LLM's with the same backend, languages and prompt, the timing check
/// with the same languages and rules.
fn to_check(all: Vec<PathBuf>, opts: &Options, state: &Path) -> anyhow::Result<(Vec<Todo>, usize)> {
    let cache = Cache::load(state)?;
    let withs = [Check::Llm, Check::Timing].map(|c| (c, checked_with(opts, c)));
    let mut cached = 0;
    let mut todo = Vec::new();
    for path in all {
        let stamp = file_stamp(&path).ok();
        let needed: Vec<Check> = withs
            .iter()
            .filter(|(_, with)| {
                opts.force
                    || !stamp.is_some_and(|(size, mtime)| {
                        cache.is_fresh(&path.to_string_lossy(), size, mtime, with)
                    })
            })
            .map(|(c, _)| *c)
            .collect();
        if needed.is_empty() {
            cached += 1;
        } else {
            todo.push((path, needed));
        }
    }
    Ok((todo, cached))
}

fn checked_with(opts: &Options, check: Check) -> CheckedWith {
    match check {
        Check::Llm => CheckedWith::new(&opts.backend.label, &opts.langs, llm::PROMPT_VERSION),
        Check::Timing => CheckedWith::timing(&opts.langs, timing::RULES_VERSION),
    }
}

/// Print every prompt `scan` would send, and run nothing. Files a scan would skip as unchanged are
/// left out here too, unless `--force`.
pub fn dry_run(
    paths: &[PathBuf],
    opts: &Options,
    state: &Path,
    out: &mut dyn Write,
    log: &mut dyn Write,
) -> anyhow::Result<()> {
    let (all, warnings) = mkv_files(paths)?;
    for w in &warnings {
        writeln!(log, "warning: {w}")?;
    }
    let total = all.len();
    let (todo, _) = to_check(all, opts, state)?;
    let todo: Vec<PathBuf> = todo
        .into_iter()
        .filter(|(_, checks)| checks.contains(&Check::Llm))
        .map(|(path, _)| path)
        .collect();
    let cached = total - todo.len();
    if cached > 0 {
        writeln!(
            log,
            "dry run: {} unchanged since an earlier scan, left out (--force to include)",
            files(cached)
        )?;
    }
    let mut chunker = Chunker::new(opts.chunk_lines);
    for path in todo {
        let p = prepare(&path, &opts.langs, &[Check::Llm]);
        if let Some(t) = p.truncated {
            writeln!(log, "{}", t.warning(&path))?;
        }
        if let Some(e) = &p.error {
            writeln!(out, "== {}: cannot read: {e}", path.display())?;
            continue;
        }
        let (_, chunks) = chunker.assign(&p.lines);
        for c in chunks {
            writeln!(
                out,
                "== {} (chunk {}, {} lines)",
                path.display(),
                c.id,
                c.lines.len()
            )?;
            let refs: Vec<&Line> = c.lines.iter().collect();
            write!(out, "{}", llm::prompt(&refs))?;
        }
    }
    Ok(())
}

struct ChunkDone {
    id: usize,
    result: Result<Vec<Suggestion>, String>,
}

struct FileJob {
    prepared: Prepared,
    /// For each line, the (chunk ID, index in chunk) whose answer covers it.
    refs: Vec<(usize, usize)>,
}

enum Msg {
    File(Box<FileJob>),
    Chunk(ChunkDone),
}

/// One chunk through the LLM: two attempts, where a failed run and an unreadable reply both
/// count as failures.
fn ask(backend: &Backend, lines: &[Line]) -> Result<Vec<Suggestion>, String> {
    let refs: Vec<&Line> = lines.iter().collect();
    let prompt = llm::prompt(&refs);
    let mut last = String::new();
    for _ in 0..2 {
        match llm::run(&backend.argv, &prompt, llm::TIMEOUT)
            .and_then(|reply| llm::parse(&reply, &refs))
        {
            Ok(reply) => return Ok(reply.suggestions),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// A worker: runs chunks through the LLM until the queue closes or the collector hangs up.
fn work(backend: &Backend, queue: &Mutex<mpsc::Receiver<Chunk>>, out: &mpsc::Sender<Msg>) {
    loop {
        let next = queue.lock().map(|rx| rx.recv());
        let Ok(Ok(chunk)) = next else { return };
        let result = ask(backend, &chunk.lines);
        if out
            .send(Msg::Chunk(ChunkDone {
                id: chunk.id,
                result,
            }))
            .is_err()
        {
            return;
        }
    }
}

/// The reader: files in order, each one's new lines queued as chunks, then the file itself.
fn read(todo: &[Todo], opts: &Options, queue: &mpsc::SyncSender<Chunk>, out: &mpsc::Sender<Msg>) {
    let mut chunker = Chunker::new(opts.chunk_lines);
    for (path, checks) in todo {
        let prepared = prepare(path, &opts.langs, checks);
        let (refs, chunks) = if prepared.error.is_none() {
            chunker.assign(&prepared.lines)
        } else {
            (Vec::new(), Vec::new())
        };
        for c in chunks {
            if queue.send(c).is_err() {
                return;
            }
        }
        if out
            .send(Msg::File(Box::new(FileJob { prepared, refs })))
            .is_err()
        {
            return;
        }
    }
}

/// The backend and languages for adding to `run` (`scan --run`): the run's own, which `llm`,
/// `llm_cmd` and `langs`, when given, must match. A run has one backend and one set of languages.
/// A run from before runs kept their languages has them in the cache's entries.
pub fn resume(
    run: &Run,
    cache: &Cache,
    llm: Option<&str>,
    llm_cmd: Option<&str>,
    langs: &[String],
) -> Result<(Backend, Vec<String>), String> {
    let header = run
        .header
        .as_ref()
        .ok_or_else(|| format!("run {} has no run record", run.name))?;
    let given = match (llm_cmd, llm) {
        (Some(cmd), _) => Some(Backend::shell(cmd)),
        (None, Some(name)) => Some(Backend::preset(name)?),
        (None, None) => None,
    };
    let backend = match given {
        Some(b) if b.label != header.backend => {
            return Err(format!(
                "run {} was scanned with {}; leave out --llm and --llm-cmd to use it",
                run.name, header.backend
            ));
        }
        Some(b) => b,
        // One this subcheck no longer knows is refused when a file needs the LLM: the command the
        // run recorded may not work as it did.
        None => Backend::from_label(&header.backend).unwrap_or_else(|_| Backend {
            label: header.backend.clone(),
            argv: Vec::new(),
            chunk_lines: llm::CHUNK_LINES,
        }),
    };
    // A run from before the languages were kept: as the cache recorded them; with nothing to say,
    // as given, else English (scan's default).
    let recorded = if header.langs.is_empty() {
        cache.langs_of(&run.name)
    } else {
        Some(header.langs.clone())
    };
    let Some(checked) = recorded else {
        let these = if langs.is_empty() {
            vec![DEFAULT_LANG.to_string()]
        } else {
            langs.to_vec()
        };
        return Ok((backend, these));
    };
    let set = |l: &[String]| CheckedWith::new("", l, 0).langs;
    if !langs.is_empty() && set(langs) != set(&checked) {
        let from = if header.langs.is_empty() {
            " (as the cache recorded it: the run is from before runs kept their languages)"
        } else {
            ""
        };
        return Err(format!(
            "run {} checked --lang {}{from}; leave out --lang to use it",
            run.name,
            checked.join(" --lang ")
        ));
    }
    Ok((backend, checked))
}

/// What a run lacks, file by file.
#[derive(Debug, Default)]
struct InRun {
    todo: Vec<Todo>,
    /// Files with every check.
    complete: usize,
    /// Files changed since the run: its findings there are stale, so they are left out.
    changed: Vec<PathBuf>,
    /// Files whose LLM check the run lacks but another run has, unchanged since, and that run.
    elsewhere: Vec<(PathBuf, String)>,
    /// Of those, the files left out for it: they lack no other check.
    only_elsewhere: usize,
    /// Runs the cache names that could not be read, and why: their files' LLM checks are made.
    unreadable: Vec<(String, String)>,
}

/// What the run `name` lacks for each file, from its records (`before`, by path): every check for a
/// file it does not have or failed on, else the checks its record lacks. An LLM check the cache has
/// from a scan of the file as it is (`llm` says how it is made) is not made again while that scan's
/// run in `runs` holds it (a good record of the file, unchanged, with the LLM check): it costs, and
/// its findings are there. The timing check is cheap, and always added.
fn in_run(
    all: Vec<PathBuf>,
    before: &HashMap<String, FileRecord>,
    (name, runs): (&str, &Path),
    cache: &Cache,
    llm: &CheckedWith,
) -> InRun {
    // Each other run's files with a good LLM record, read once.
    let mut held: HashMap<String, HashMap<String, (u64, Timestamp)>> = HashMap::new();
    let mut unreadable = Vec::new();
    let every = [Check::Llm, Check::Timing];
    let mut out = InRun::default();
    for path in all {
        let key = path.to_string_lossy().into_owned();
        let stamp = file_stamp(&path).ok();
        let mut needed = match before.get(&key) {
            Some(f) if f.error.is_none() => {
                if stamp != Some((f.size, f.mtime)) {
                    out.changed.push(path);
                    continue;
                }
                every
                    .into_iter()
                    .filter(|c| !f.checks.contains(c))
                    .collect()
            }
            _ => every.to_vec(),
        };
        let mut skipped = false;
        if needed.contains(&Check::Llm)
            && let Some((size, mtime)) = stamp
            && let Some(other) = cache.run_of(&key, size, mtime, llm)
            && other != name
            && held
                .entry(other.to_string())
                .or_insert_with(|| {
                    llm_records(&runs.join(format!("{other}.jsonl"))).unwrap_or_else(|e| {
                        unreadable.push((other.to_string(), e.to_string()));
                        HashMap::new()
                    })
                })
                .get(&key)
                == Some(&(size, mtime))
        {
            needed.retain(|c| *c != Check::Llm);
            out.elsewhere.push((path.clone(), other.to_string()));
            skipped = true;
        }
        match (needed.is_empty(), skipped) {
            (true, true) => out.only_elsewhere += 1,
            (true, false) => out.complete += 1,
            (false, _) => out.todo.push((path, needed)),
        }
    }
    out.unreadable = unreadable;
    out
}

/// What a scan writes to: the files to read and their checks, the run, the next finding's ID, and
/// with `--run` the run's records of its files.
struct Start {
    todo: Vec<Todo>,
    writer: RunWriter,
    next_finding: u64,
    before: HashMap<String, FileRecord>,
}

/// A new run for the files the cache says need a check; `None` when none do.
fn start_new(
    all: Vec<PathBuf>,
    opts: &Options,
    state: &Path,
    summary: &mut Summary,
) -> anyhow::Result<Option<Start>> {
    let (todo, cached) = to_check(all, opts, state)?;
    summary.cached = cached;
    // No run file for a scan with nothing to check: an empty run would become the newest and hide
    // the last real one from a plain `subcheck list`.
    if todo.is_empty() {
        return Ok(None);
    }
    let mut writer = RunWriter::create(state)?;
    writer.lock()?;
    writer.append(&[Record::Run(RunHeader {
        started: Timestamp::now(),
        backend: opts.backend.label.clone(),
        command: opts.backend.argv.clone(),
        prompt_version: llm::PROMPT_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
        langs: opts.langs.clone(),
    })])?;
    Ok(Some(Start {
        todo,
        writer,
        next_finding: 1,
        before: HashMap::new(),
    }))
}

/// Adding to the run file `path` (`--run`) what it lacks; `None` when it lacks nothing.
fn start_adding(
    path: &Path,
    all: Vec<PathBuf>,
    opts: &Options,
    state: &Path,
    summary: &mut Summary,
    log: &mut dyn Write,
) -> anyhow::Result<Option<Start>> {
    let mut writer = RunWriter::open_locked(path)?;
    let run = store::read_run(path)?;
    let next_finding = run.next_finding_id();
    let Run {
        name,
        header,
        files,
        ..
    } = run;
    let before: HashMap<String, FileRecord> =
        files.into_iter().map(|f| (f.path.clone(), f)).collect();
    let cache = Cache::load(state)?;
    let llm = checked_with(opts, Check::Llm);
    let found = in_run(all, &before, (&name, &state.join("runs")), &cache, &llm);
    summary.cached = found.complete;
    summary.changed = found.changed.len();
    summary.elsewhere = found.only_elsewhere;
    summary.run.clone_from(&name);
    for path in &found.changed {
        writeln!(
            log,
            "{}: changed since run {}; scan it into a new run",
            path.display(),
            name
        )?;
    }
    let todo = found.todo;
    if todo.iter().any(|(_, c)| c.contains(&Check::Llm)) {
        let prompt = header.as_ref().map(|h| h.prompt_version);
        if prompt != Some(llm::PROMPT_VERSION) {
            anyhow::bail!(
                "run {} was made with prompt version {}, this subcheck has {}: scan the files \
                 it lacks into a new run",
                name,
                prompt.map_or("?".into(), |p| p.to_string()),
                llm::PROMPT_VERSION
            );
        }
        if opts.backend.argv.is_empty() {
            anyhow::bail!(
                "run {} was scanned with {}, which this subcheck does not know: scan the files \
                 it lacks into a new run",
                name,
                opts.backend.label
            );
        }
    }
    for (other, e) in &found.unreadable {
        writeln!(
            log,
            "run {other} cannot be read ({e}): the LLM checks the cache says it has are made"
        )?;
    }
    for (path, other) in &found.elsewhere {
        writeln!(
            log,
            "{}: its LLM check is in run {other}, unchanged since; not made again",
            path.display()
        )?;
    }
    if todo.is_empty() {
        return Ok(None);
    }
    // A run from before the languages were kept: say which it is added to with, so the next
    // `--run` takes the same (the latest run record wins).
    if let Some(h) = header.as_ref().filter(|h| h.langs.is_empty()) {
        writer.append(&[Record::Run(RunHeader {
            langs: opts.langs.clone(),
            ..h.clone()
        })])?;
    }
    Ok(Some(Start {
        todo,
        writer,
        next_finding,
        before,
    }))
}

/// The files a run file holds a good record of with the LLM check, and their size and mtime then.
/// A run that is gone holds none.
fn llm_records(path: &Path) -> io::Result<HashMap<String, (u64, Timestamp)>> {
    match store::read_run(path) {
        Ok(run) => Ok(run
            .files
            .into_iter()
            .filter(|f| f.error.is_none() && f.checks.contains(&Check::Llm))
            .map(|f| (f.path, (f.size, f.mtime)))
            .collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(e),
    }
}

pub fn run(
    paths: &[PathBuf],
    opts: &Options,
    state: &Path,
    log: &mut dyn Write,
) -> anyhow::Result<Summary> {
    let (all, warnings) = mkv_files(paths)?;
    for w in &warnings {
        writeln!(log, "warning: {w}")?;
    }
    let mut summary = Summary {
        files: all.len(),
        ..Summary::default()
    };
    let start = match &opts.into {
        None => start_new(all, opts, state, &mut summary)?,
        Some(path) => start_adding(path, all, opts, state, &mut summary, log)?,
    };
    let Some(Start {
        todo,
        writer,
        next_finding,
        before,
    }) = start
    else {
        return Ok(summary);
    };
    summary.run.clone_from(&writer.name);
    let mut collector = Collector {
        writer,
        state,
        opts,
        summary,
        total: todo.len(),
        done: 0,
        next_finding,
        before,
        answers: HashMap::new(),
        waiting: Vec::new(),
    };
    thread::scope(|scope| -> anyhow::Result<()> {
        // Created inside the scope so an early return drops the receivers: workers and the
        // reader then fail their next send and stop, instead of running the rest of the queue.
        let (queue_tx, queue_rx) = mpsc::sync_channel::<Chunk>(opts.jobs.max(1) * 2);
        let queue_rx = Arc::new(Mutex::new(queue_rx));
        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        for _ in 0..opts.jobs.max(1) {
            let queue_rx = Arc::clone(&queue_rx);
            let msg_tx = msg_tx.clone();
            scope.spawn(move || work(&opts.backend, &queue_rx, &msg_tx));
        }
        let todo = &todo;
        scope.spawn(move || read(todo, opts, &queue_tx, &msg_tx));
        collector.collect(&msg_rx, log)
    })?;
    Ok(collector.summary)
}

/// Writes each file's records the moment every one of its lines has an answer.
struct Collector<'a> {
    writer: RunWriter,
    state: &'a Path,
    opts: &'a Options,
    summary: Summary,
    total: usize,
    done: usize,
    next_finding: u64,
    /// With `--run`: the run's records of its files, whose checks a new record adds to.
    before: HashMap<String, FileRecord>,
    answers: HashMap<usize, Result<Vec<Suggestion>, String>>,
    waiting: Vec<FileJob>,
}

impl Collector<'_> {
    /// Runs until the reader and every worker have hung up.
    fn collect(&mut self, rx: &mpsc::Receiver<Msg>, log: &mut dyn Write) -> anyhow::Result<()> {
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::File(job) => self.waiting.push(*job),
                Msg::Chunk(c) => {
                    self.answers.insert(c.id, c.result);
                }
            }
            while let Some(i) = self.waiting.iter().position(|j| {
                j.refs
                    .iter()
                    .all(|(chunk, _)| self.answers.contains_key(chunk))
            }) {
                let job = self.waiting.remove(i);
                self.finish(job, log)?;
            }
        }
        for job in std::mem::take(&mut self.waiting) {
            let p = job.prepared;
            if let Some(t) = p.truncated {
                writeln!(log, "{}", t.warning(&p.path))?;
            }
            self.summary.failed += 1;
            let error = "no answer for some lines".to_string();
            let path = p.path.to_string_lossy().into_owned();
            if self.earlier(&path).is_none() {
                self.writer.append(&[Record::File(FileRecord {
                    path,
                    size: p.size,
                    mtime: p.mtime,
                    error: Some(error.clone()),
                    tracks: p.tracks,
                    lines: p.lines.len(),
                    truncated: p.truncated,
                    checks: p.checks,
                })])?;
            }
            writeln!(log, "{}: FAILED: {error}", p.path.display())?;
        }
        Ok(())
    }

    fn finish(&mut self, job: FileJob, log: &mut dyn Write) -> anyhow::Result<()> {
        let FileJob { prepared: p, refs } = job;
        self.done += 1;
        let name = p
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let progress = format!("[{}/{}] {name}", self.done, self.total);
        let path = p.path.to_string_lossy().into_owned();
        if let Some(t) = p.truncated {
            writeln!(log, "{}", t.warning(&p.path))?;
        }
        let llm_error = refs
            .iter()
            .find_map(|(chunk, _)| self.answers[chunk].as_ref().err())
            .map(|e| format!("LLM: {e}"));
        let error = p.error.clone().or(llm_error);
        // A check added to a file the run has: the new record keeps what the old one says of the
        // checks not made now.
        let earlier = self.earlier(&path);
        let keep = earlier.is_some();
        let mut checks = p.checks.clone();
        checks.extend(earlier.iter().flat_map(|f| f.checks.iter().copied()));
        checks.sort();
        checks.dedup();
        let lines = match earlier {
            Some(f) if !p.checks.contains(&Check::Llm) => f.lines,
            _ => p.lines.len(),
        };
        let file = Record::File(FileRecord {
            path: path.clone(),
            size: p.size,
            mtime: p.mtime,
            error: error.clone(),
            tracks: p.tracks,
            lines,
            truncated: p.truncated,
            checks,
        });
        if let Some(e) = error {
            self.summary.failed += 1;
            if !keep {
                self.writer.append(&[file])?;
            }
            writeln!(log, "{progress}: FAILED: {e}")?;
        } else {
            let mut findings = findings(
                &self.answers,
                &p.lines,
                &refs,
                &path,
                &mut self.next_finding,
            );
            let timing = p.timing.len();
            for f in p.timing {
                findings.push(Finding {
                    id: self.next_finding,
                    ..f
                });
                self.next_finding += 1;
            }
            let found = findings.len();
            let mut records = vec![file];
            records.extend(findings.into_iter().map(Record::Finding));
            self.writer.append(&records)?;
            if !p.skipped_tracks && p.truncated.is_none() {
                self.cache(&path, p.size, p.mtime, &p.checks)?;
            }
            self.summary.checked += 1;
            self.summary.findings += found;
            self.summary.timing += timing;
            let of_timing = if timing > 0 {
                format!(" ({timing} timing)")
            } else {
                String::new()
            };
            if p.checks.contains(&Check::Llm) {
                writeln!(
                    log,
                    "{progress}: {} lines, {found} findings{of_timing}",
                    p.lines.len()
                )?;
            } else {
                writeln!(log, "{progress}: timing only, {found} findings")?;
            }
        }
        for note in &p.timing_notes {
            writeln!(log, "    {note}")?;
        }
        if self.opts.verbose {
            for note in &p.notes {
                writeln!(log, "    {note}")?;
            }
        }
        Ok(())
    }
}

impl Collector<'_> {
    /// With `--run`, the run's good record of the file: a new record adds its checks to it, and a
    /// failure adding one leaves it standing, so the next `--run` adds only the missing check
    /// again (a failed record would have it redo them all, and the LLM's findings twice).
    fn earlier(&self, path: &str) -> Option<&FileRecord> {
        self.before.get(path).filter(|f| f.error.is_none())
    }

    /// Each check made of a file, so the next scan skips it. Written once its records are in the
    /// run.
    fn cache(&self, path: &str, size: u64, mtime: Timestamp, checks: &[Check]) -> io::Result<()> {
        for &check in checks {
            Cache::append(
                self.state,
                &CacheEntry {
                    path: path.to_string(),
                    size,
                    mtime,
                    with: checked_with(self.opts, check),
                    run: self.writer.name.clone(),
                },
            )?;
        }
        Ok(())
    }
}

/// One finding per distinct suggestion for each line, with up to two lines of context either side.
fn findings(
    answers: &HashMap<usize, Result<Vec<Suggestion>, String>>,
    lines: &[Line],
    refs: &[(usize, usize)],
    path: &str,
    next_id: &mut u64,
) -> Vec<Finding> {
    let context = |from: usize, to: usize| -> Vec<String> {
        let to = to.min(lines.len());
        lines[from.min(to)..to]
            .iter()
            .map(|l| l.text.clone())
            .collect()
    };
    let mut out = Vec::new();
    for (i, (line, (chunk, index))) in lines.iter().zip(refs).enumerate() {
        let Some(Ok(suggestions)) = answers.get(chunk) else {
            continue;
        };
        let mut seen: Vec<&str> = Vec::new();
        for s in suggestions.iter().filter(|s| s.index == *index) {
            if seen.contains(&s.corrected.as_str()) {
                continue;
            }
            seen.push(&s.corrected);
            out.push(Finding {
                id: *next_id,
                file: path.to_string(),
                style: line.style.clone(),
                original: line.text.clone(),
                suggestion: s.corrected.clone(),
                reason: s.reason.clone(),
                source: "llm".into(),
                occurrences: line.occurrences.clone(),
                before: context(i.saturating_sub(2), i),
                after: context(i + 1, i + 3),
                timing: None,
            });
            *next_id += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Occurrence;

    fn line(text: &str) -> Line {
        Line {
            style: "D".into(),
            text: text.into(),
            occurrences: vec![Occurrence {
                track: 0,
                start_ms: 0,
                end_ms: 1,
            }],
        }
    }

    fn run_with(backend: &str, langs: &[&str]) -> Run {
        Run {
            name: "r".into(),
            header: Some(RunHeader {
                started: Timestamp::UNIX_EPOCH,
                backend: backend.into(),
                command: Vec::new(),
                prompt_version: llm::PROMPT_VERSION,
                version: String::new(),
                langs: langs.iter().map(|l| (*l).to_string()).collect(),
            }),
            ..Run::default()
        }
    }

    fn langs(l: &[&str]) -> Vec<String> {
        l.iter().map(|l| (*l).to_string()).collect()
    }

    #[test]
    fn adding_to_a_run_takes_its_backend_and_languages_unless_they_are_given_alike() {
        let none = Cache::default();
        let resume = |run: &Run, llm, llm_cmd, langs: &[String]| {
            super::resume(run, &none, llm, llm_cmd, langs)
        };
        let run = run_with("claude", &["eng", "und"]);
        let (b, l) = resume(&run, None, None, &[]).unwrap();
        assert_eq!((b.label.as_str(), l), ("claude", langs(&["eng", "und"])));
        // The same, given again (in another order).
        let (b, _) = resume(&run, Some("claude"), None, &langs(&["und", "eng"])).unwrap();
        assert_eq!(b.label, "claude");
        assert_eq!(
            resume(&run, Some("gpt"), None, &[]).unwrap_err(),
            "run r was scanned with claude; leave out --llm and --llm-cmd to use it"
        );
        assert_eq!(
            resume(&run, None, None, &langs(&["eng"])).unwrap_err(),
            "run r checked --lang eng --lang und; leave out --lang to use it"
        );
        // A command LLM comes back from its label.
        let cmd = run_with("cmd:my-llm --x", &["eng"]);
        let (b, _) = resume(&cmd, None, None, &[]).unwrap();
        assert_eq!(b.argv, ["sh", "-c", "my-llm --x"]);
        // A run from before the languages were kept, with nothing in the cache to say: as given,
        // else English.
        let old = run_with("claude", &[]);
        assert_eq!(resume(&old, None, None, &[]).unwrap().1, ["eng"]);
        assert_eq!(
            resume(&old, None, None, &langs(&["und"])).unwrap().1,
            ["und"]
        );
        assert!(resume(&Run::default(), None, None, &[]).is_err());
        // A backend this subcheck does not know: no command, refused later if a file needs it,
        // whatever command the run recorded.
        let mut gone = run_with("gemini", &["eng"]);
        if let Some(h) = &mut gone.header {
            h.command = vec!["gemini".into(), "-p".into()];
        }
        let (b, _) = resume(&gone, None, None, &[]).unwrap();
        assert_eq!((b.label.as_str(), b.argv.len()), ("gemini", 0));
    }

    #[test]
    fn an_old_runs_languages_come_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let entry = |run: &str, check: Check| CacheEntry {
            path: format!("/m/{run}.mkv"),
            size: 1,
            mtime: Timestamp::UNIX_EPOCH,
            with: CheckedWith {
                check,
                ..CheckedWith::new("claude", &langs(&["eng", "und"]), 1)
            },
            run: run.into(),
        };
        // Its timing check, of other languages, says nothing of what its LLM checks were of.
        let timing = CacheEntry {
            path: "/m/b.mkv".into(),
            with: CheckedWith::timing(&langs(&["jpn"]), 1),
            ..entry("r", Check::Timing)
        };
        Cache::append(dir.path(), &timing).unwrap();
        let old = run_with("claude", &[]);
        let cache = Cache::load(dir.path()).unwrap();
        assert_eq!(resume(&old, &cache, None, None, &[]).unwrap().1, ["eng"]);
        Cache::append(dir.path(), &entry("r", Check::Llm)).unwrap();
        let cache = Cache::load(dir.path()).unwrap();
        assert_eq!(
            resume(&old, &cache, None, None, &[]).unwrap().1,
            ["eng", "und"]
        );
        assert_eq!(
            resume(&old, &cache, None, None, &langs(&["eng"])).unwrap_err(),
            "run r checked --lang eng --lang und (as the cache recorded it: the run is from \
             before runs kept their languages); leave out --lang to use it"
        );
    }

    #[test]
    fn an_llm_check_is_not_made_again_only_while_another_run_holds_the_file_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.mkv");
        std::fs::write(&file, b"x").unwrap();
        let key = file.to_string_lossy().into_owned();
        let (size, mtime) = file_stamp(&file).unwrap();
        let state = dir.path().join("state");
        let runs = state.join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        let llm = CheckedWith::new("claude", &langs(&["eng"]), 1);
        let entry = CacheEntry {
            path: key.clone(),
            size,
            mtime,
            with: llm.clone(),
            run: "y".into(),
        };
        Cache::append(&state, &entry).unwrap();
        let cache = Cache::load(&state).unwrap();
        let record = |size| {
            let r = Record::File(FileRecord {
                path: key.clone(),
                size,
                mtime,
                error: None,
                tracks: vec![],
                lines: 1,
                truncated: None,
                checks: vec![Check::Llm],
            });
            serde_json::to_string(&r).unwrap() + "\n"
        };
        let y = runs.join("y.jsonl");
        let needs = |adding_to: &str| {
            let found = in_run(
                vec![file.clone()],
                &HashMap::new(),
                (adding_to, &runs),
                &cache,
                &llm,
            );
            let todo: Vec<Vec<Check>> = found.todo.into_iter().map(|(_, c)| c).collect();
            (todo, found.elsewhere.len(), found.unreadable.len())
        };
        let both = vec![vec![Check::Llm, Check::Timing]];
        // Run y holds the file as it is: only the timing check.
        std::fs::write(&y, record(size)).unwrap();
        assert_eq!(needs("x"), (vec![vec![Check::Timing]], 1, 0));
        // Not when y is the run being added to.
        assert_eq!(needs("y"), (both.clone(), 0, 0));
        // Its record is of an older copy of the file.
        std::fs::write(&y, record(size + 1)).unwrap();
        assert_eq!(needs("x"), (both.clone(), 0, 0));
        // It cannot be read: said so.
        std::fs::write(&y, b"\xff\n").unwrap();
        assert_eq!(needs("x"), (both.clone(), 0, 1));
        // It is gone.
        std::fs::remove_file(&y).unwrap();
        assert_eq!(needs("x"), (both, 0, 0));
    }

    #[test]
    fn chunker_splits_by_size_and_sends_each_text_once() {
        let mut c = Chunker::new(2);
        let (refs, chunks) = c.assign(&[line("a"), line("b"), line("c")]);
        assert_eq!(refs, [(0, 0), (0, 1), (1, 0)]);
        assert_eq!(
            chunks.iter().map(|c| c.lines.len()).collect::<Vec<_>>(),
            [2, 1]
        );
        // A second file: "b" was already sent, "d" is new and starts a chunk of its own.
        let (refs, chunks) = c.assign(&[line("b"), line("d")]);
        assert_eq!(refs, [(0, 1), (2, 0)]);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].lines[0].text, "d");
    }

    fn track(language: &str, bcp47: Option<&str>) -> Track {
        Track {
            id: 0,
            number: 1,
            uid: 1,
            kind: TRACK_TYPE_SUBTITLE,
            codec: "S_TEXT/ASS".into(),
            name: None,
            language: language.into(),
            language_element: true,
            language_bcp47: bcp47.map(Into::into),
            default: true,
            forced: false,
            default_duration: None,
            codec_private: Vec::new(),
            encodings: Vec::new(),
            flags: crate::mkv::Flags::default(),
            video: None,
            audio: None,
        }
    }

    #[test]
    fn language_matches_by_iso_639_2_or_bcp_47() {
        let eng = ["eng".to_string()];
        assert!(wanted_language(&track("eng", None), &eng));
        assert!(wanted_language(&track("und", Some("en-US")), &eng));
        assert!(wanted_language(&track("und", Some("EN-us")), &eng));
        assert!(!wanted_language(&track("und", Some("und")), &eng));
        assert!(!wanted_language(&track("jpn", Some("ja")), &eng));
        assert!(wanted_language(
            &track("und", Some("und")),
            &["und".to_string()]
        ));
        // LanguageBCP47 overrides Language, which defaults to "eng" when absent.
        assert!(!wanted_language(&track("eng", Some("ja")), &eng));
    }

    #[test]
    fn an_unreadable_directory_is_skipped_with_a_warning() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root reads a 0o000 directory anyway (CI containers run as root)
        }
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("locked")).unwrap();
        std::fs::write(d.join("a.mkv"), "").unwrap();
        let mode = |m| std::fs::Permissions::from_mode(m);
        std::fs::set_permissions(d.join("locked"), mode(0o000)).unwrap();
        let result = mkv_files(&[d.to_path_buf()]);
        std::fs::set_permissions(d.join("locked"), mode(0o755)).unwrap();
        let (found, warnings) = result.unwrap();
        assert_eq!(found, [d.join("a.mkv")]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("locked"), "{warnings:?}");
    }

    #[test]
    fn mkv_files_walks_sorted_skipping_hidden_and_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("Show/Specials")).unwrap();
        std::fs::create_dir_all(d.join(".Recycle.Bin")).unwrap();
        for f in [
            "Show/b.mkv",
            "Show/a.MKV",
            "Show/Specials/c.mkv",
            "Show/notes.txt",
            ".Recycle.Bin/x.mkv",
            "Show/.d.mkv",
        ] {
            std::fs::write(d.join(f), "").unwrap();
        }
        let (found, warnings) = mkv_files(&[d.to_path_buf()]).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let names: Vec<_> = found
            .iter()
            .map(|p| p.strip_prefix(d).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["Show/Specials/c.mkv", "Show/a.MKV", "Show/b.mkv"]);
        assert!(mkv_files(&[d.join("missing")]).is_err());
    }
}
