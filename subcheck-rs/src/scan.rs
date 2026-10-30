//! `subcheck scan`: find MKVs, read their text subtitle tracks, send the lines to the LLM in
//! chunks, and record every suggested fix in the store.
//!
//! One reader thread reads files in order (the array's disks are the bottleneck, so never two at
//! once), `-j` workers run the LLM, and the calling thread collects: it saves a file, in one
//! transaction, the moment every one of its lines has an answer, and each LLM call's usage as it
//! comes.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use jiff::Timestamp;

use crate::blocks::{self, ReadPath};
use crate::list::shell_quote;
use crate::llm::{self, Answer, Backend, Spent, Suggestion, Usage, Used};
use crate::mkv::{self, Layout, TRACK_TYPE_SUBTITLE, Track, Truncated};
use crate::store::{
    self, Attempt, Check, CheckedWith, FileRecord, Finding, LlmCall, Made, Record, RunHeader,
    Saved, ScanRow, Store, TrackRecord,
};
use crate::subs::Kind;
use crate::text::{self, Line};
use crate::timing::{self, record, shared};

/// The subtitle languages a scan checks unless told otherwise.
pub const DEFAULT_LANG: &str = "eng";

/// LLM calls a scan runs at once, unless `--jobs` says.
pub const DEFAULT_JOBS: usize = 2;

pub struct Options {
    pub backend: Backend,
    pub chunk_lines: usize,
    pub jobs: usize,
    /// ISO 639-2 codes; a track matches by `Language` or by its BCP 47 primary subtag.
    pub langs: Vec<String>,
    pub force: bool,
    pub verbose: bool,
    /// The run to add to (`--run`), by name, instead of starting a run.
    pub into: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Summary {
    pub run: String,
    pub files: usize,
    pub checked: usize,
    pub failed: usize,
    pub cached: usize,
    pub findings: usize,
    /// Of the findings, those of the timing check.
    pub timing: usize,
    /// What this scan's LLM calls used.
    pub spent: Spent,
    /// The paths the scan was given, absolute: what `subcheck review` takes to show their files.
    pub paths: Vec<PathBuf>,
}

/// What a scan tells the hook [`execute`] is given, as it goes: enough for review's screen to show
/// each file's progress, and to take each record as the store shows it.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The files to check, in the order they are read: once, first.
    Queued(Vec<PathBuf>),
    /// The reader has started on a file.
    Reading(PathBuf),
    /// Of the LLM chunks that answer a file's lines, how many have answered: said once the file is
    /// read, then as each answers. A line already sent for an earlier file counts that file's
    /// chunk. Not said of a file with no lines for the LLM.
    Asking {
        path: PathBuf,
        answered: usize,
        chunks: usize,
    },
    /// Records just saved, in order: an LLM call's usage, or a file's record (of the checks this
    /// scan made, or of its failure), its findings, and the decisions carried over to them.
    Appended(Vec<Record>),
    /// A file done, after its records: why it failed, if it did; its findings; and the lines just
    /// written to the log for it.
    Finished {
        path: PathBuf,
        error: Option<String>,
        findings: usize,
        lines: Vec<String>,
    },
}

/// What [`plan`] found.
pub enum Planned {
    /// Nothing to check; the summary says why (no files, every one unchanged since an earlier
    /// scan, or with `--run`, nothing the run needs).
    Nothing(Summary),
    Scan(Plan),
}

/// A scan ready to run: its run made (or, with `--run`, found), with every file it names.
pub struct Plan {
    start: Start,
    summary: Summary,
}

impl Plan {
    /// The run the scan records under.
    pub fn scan(&self) -> &ScanRow {
        &self.start.scan
    }

    /// The files the scan checks, in the order it reads them, each with the checks it makes.
    pub fn todo(&self) -> &[(PathBuf, Vec<Check>)] {
        &self.start.todo
    }
}

/// `paths` expanded to MKV files: files as given, directories walked recursively for `*.mkv`
/// (any case), skipping hidden entries (`.Recycle.Bin`, `.Trash-1000`, ...). Sorted, absolute.
/// Also returns a warning for each directory or entry that could not be read: it is skipped, so
/// one locked directory cannot stop a scan of a whole share.
pub fn mkv_files(paths: &[PathBuf]) -> io::Result<(Vec<PathBuf>, Vec<String>)> {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for p in paths {
        let p = crate::store::absolute(p)?;
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
    /// The file is shorter than its Segment says. Its check is never fresh, so every scan says so
    /// until the copy is replaced; nor is one with a track that could not be read.
    truncated: Option<Truncated>,
    /// Shown with `-v`.
    notes: Vec<String>,
}

/// A file's size and modification time: what a check's freshness and review compare.
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
    let mut events: Vec<(usize, Vec<text::Event>)> = Vec::new();
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
    tracks: &[(usize, Vec<text::Event>)],
) -> Result<(Vec<Finding>, Vec<String>), String> {
    let video = timing::video(layout)?;
    let measured: Vec<timing::Analysis> = tracks
        .iter()
        .map(|(_, events)| timing::analyse(&video, events))
        .collect();
    let analysed: Vec<(usize, &[text::Event], &timing::Analysis)> = tracks
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

/// Each file with the checks it needs, and how many files need none: each file's current check
/// of each kind is fresh ([`Store::is_fresh`]), the LLM's made with the same backend, languages
/// and prompt, the timing check's with the same languages and rules. `--force` needs them all.
fn to_check(all: &[PathBuf], opts: &Options, store: &Store) -> anyhow::Result<(Vec<Todo>, usize)> {
    let withs = [Check::Llm, Check::Timing].map(|c| (c, checked_with(opts, c)));
    let mut cached = 0;
    let mut todo = Vec::new();
    for path in all {
        let key = path.to_string_lossy();
        let stamp = file_stamp(path).ok().filter(|_| !opts.force);
        let mut needed = Vec::new();
        for (check, with) in &withs {
            let fresh = match stamp {
                Some((size, mtime)) => store.is_fresh(&key, size, mtime, with)?,
                None => false,
            };
            if !fresh {
                needed.push(*check);
            }
        }
        if needed.is_empty() {
            cached += 1;
        } else {
            todo.push((path.clone(), needed));
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
    store: &Store,
    out: &mut dyn Write,
    log: &mut dyn Write,
) -> anyhow::Result<()> {
    let (all, warnings) = mkv_files(paths)?;
    for w in &warnings {
        writeln!(log, "warning: {w}")?;
    }
    let total = all.len();
    let (todo, _) = to_check(&all, opts, store)?;
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

/// A chunk on its way to a worker, with the file whose lines it carries.
struct Queued {
    chunk: Chunk,
    file: Arc<str>,
}

struct ChunkDone {
    id: usize,
    result: Result<Vec<Suggestion>, String>,
    /// Each call made for it to count (see [`ask`]).
    calls: Vec<LlmCall>,
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
/// count as failures. Also each call to count, with what it used when its backend said: a failed
/// one costs too.
fn ask(backend: &Backend, lines: &[Line]) -> (Result<Vec<Suggestion>, String>, Vec<Option<Usage>>) {
    let refs: Vec<&Line> = lines.iter().collect();
    let prompt = llm::prompt(&refs);
    let mut calls = Vec::new();
    let mut last = String::new();
    for _ in 0..2 {
        let Answer { reply, used } = backend.call(&prompt, llm::TIMEOUT);
        match used {
            Used::Reported(usage) => calls.push(Some(usage)),
            Used::Unreported => calls.push(None),
            Used::Untracked => {}
        }
        match reply.and_then(|reply| llm::parse(&reply, &refs)) {
            Ok(reply) => return (Ok(reply.suggestions), calls),
            Err(e) => last = e,
        }
    }
    (Err(last), calls)
}

/// A worker: runs chunks through the LLM until the queue closes or the collector hangs up.
fn work(backend: &Backend, queue: &Mutex<mpsc::Receiver<Queued>>, out: &mpsc::Sender<Msg>) {
    loop {
        let next = queue.lock().map(|rx| rx.recv());
        let Ok(Ok(Queued { chunk, file })) = next else {
            return;
        };
        let (result, used) = ask(backend, &chunk.lines);
        let calls = used
            .into_iter()
            .map(|usage| LlmCall {
                file: file.to_string(),
                lines: chunk.lines.len(),
                usage,
            })
            .collect();
        if out
            .send(Msg::Chunk(ChunkDone {
                id: chunk.id,
                result,
                calls,
            }))
            .is_err()
        {
            return;
        }
    }
}

/// The reader: files in order, each one's new lines queued as chunks, then the file itself.
fn read(
    todo: &[Todo],
    opts: &Options,
    queue: &mpsc::SyncSender<Queued>,
    out: &mpsc::Sender<Msg>,
    report: &(dyn Fn(Event) + Sync),
) {
    let mut chunker = Chunker::new(opts.chunk_lines);
    for (path, checks) in todo {
        report(Event::Reading(path.clone()));
        let prepared = prepare(path, &opts.langs, checks);
        let (refs, chunks) = if prepared.error.is_none() {
            chunker.assign(&prepared.lines)
        } else {
            (Vec::new(), Vec::new())
        };
        let file: Arc<str> = path.to_string_lossy().into();
        for chunk in chunks {
            let file = Arc::clone(&file);
            if queue.send(Queued { chunk, file }).is_err() {
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

/// The backend and languages for adding to the run `scan` (`scan --run`): the run's own, which
/// `llm`, `llm_cmd` and `langs`, when given, must match. A run has one backend and one set of
/// languages.
pub fn resume(
    scan: &ScanRow,
    llm: Option<&str>,
    llm_cmd: Option<&str>,
    langs: &[String],
) -> Result<(Backend, Vec<String>), String> {
    let header = &scan.header;
    let given = match (llm_cmd, llm) {
        (Some(cmd), _) => Some(Backend::shell(cmd)),
        (None, Some(name)) => Some(Backend::preset(name)?),
        (None, None) => None,
    };
    let backend = match given {
        Some(b) if b.label != header.backend => {
            return Err(format!(
                "run {} was scanned with {}; leave out --llm and --llm-cmd to use it",
                scan.name, header.backend
            ));
        }
        Some(b) => b,
        // One this subcheck no longer knows is refused when a file needs the LLM: the command the
        // run recorded may not work as it did.
        None => Backend::from_label(&header.backend).unwrap_or_else(|_| Backend {
            label: header.backend.clone(),
            argv: Vec::new(),
            chunk_lines: llm::CHUNK_LINES,
            output: llm::Output::Text,
        }),
    };
    let set = |l: &[String]| CheckedWith::new("", l, 0).langs;
    if !langs.is_empty() && set(langs) != set(&header.langs) {
        return Err(format!(
            "run {} checked --lang {}; leave out --lang to use it",
            scan.name,
            header.langs.join(" --lang ")
        ));
    }
    Ok((backend, header.langs.clone()))
}

/// What a scan records under: the files to read and their checks, and the run.
struct Start {
    todo: Vec<Todo>,
    scan: ScanRow,
}

/// A new run for the files that need a check, naming every file found; `None` when none needs
/// one.
fn start_new(
    all: &[PathBuf],
    opts: &Options,
    store: &mut Store,
    summary: &mut Summary,
) -> anyhow::Result<Option<Start>> {
    let (todo, cached) = to_check(all, opts, store)?;
    summary.cached = cached;
    // No run for a scan with nothing to check: an empty run would become the newest and hide the
    // last real one from a plain `subcheck list`.
    if todo.is_empty() {
        return Ok(None);
    }
    let header = RunHeader {
        started: Timestamp::now(),
        backend: opts.backend.label.clone(),
        command: opts.backend.argv.clone(),
        prompt_version: llm::PROMPT_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
        langs: opts.langs.clone(),
    };
    let scan = store.begin_scan(&header, all)?;
    Ok(Some(Start { todo, scan }))
}

/// Adding to the run `name` (`--run`): every file found joins it, and the checks they need are
/// made under it; `None` when none needs one.
fn start_adding(
    name: &str,
    all: &[PathBuf],
    opts: &Options,
    store: &mut Store,
    summary: &mut Summary,
) -> anyhow::Result<Option<Start>> {
    let scan = store
        .scan_named(name)?
        .ok_or_else(|| anyhow::anyhow!("no run named {name}"))?;
    summary.run.clone_from(&scan.name);
    let (todo, cached) = to_check(all, opts, store)?;
    summary.cached = cached;
    if todo.iter().any(|(_, c)| c.contains(&Check::Llm)) {
        let prompt = scan.header.prompt_version;
        if prompt != llm::PROMPT_VERSION {
            anyhow::bail!(
                "run {} was made with prompt version {prompt}, this subcheck has {}: scan the \
                 files it lacks into a new run",
                scan.name,
                llm::PROMPT_VERSION
            );
        }
        if opts.backend.argv.is_empty() {
            anyhow::bail!(
                "run {} was scanned with {}, which this subcheck does not know: scan the files \
                 it lacks into a new run",
                scan.name,
                opts.backend.label
            );
        }
    }
    store.add_files(scan.id, all)?;
    if todo.is_empty() {
        return Ok(None);
    }
    Ok(Some(Start { todo, scan }))
}

/// The files under `paths` and what each needs, and the run they go under: a new one, or with
/// `--run` (`opts.into`) the run named. Every file found is the run's, skipped ones too, before
/// checking starts. Warnings go to `log`.
pub fn plan(
    paths: &[PathBuf],
    opts: &Options,
    store: &mut Store,
    log: &mut dyn Write,
) -> anyhow::Result<Planned> {
    let (all, warnings) = mkv_files(paths)?;
    for w in &warnings {
        writeln!(log, "warning: {w}")?;
    }
    let mut summary = Summary {
        files: all.len(),
        paths: paths
            .iter()
            .map(|p| store::absolute(p))
            .collect::<io::Result<_>>()?,
        ..Summary::default()
    };
    let start = match &opts.into {
        None => start_new(&all, opts, store, &mut summary)?,
        Some(name) => start_adding(name, &all, opts, store, &mut summary)?,
    };
    Ok(match start {
        None => Planned::Nothing(summary),
        Some(start) => {
            summary.run.clone_from(&start.scan.name);
            Planned::Scan(Plan { start, summary })
        }
    })
}

/// Run `plan`: one reader thread, `-j` workers, and the collector on this thread, which saves
/// each file the moment its lines are all answered. `report` hears each step (see [`Event`]),
/// from the reader's thread and this one.
pub fn execute(
    plan: Plan,
    opts: &Options,
    store: &mut Store,
    log: &mut dyn Write,
    report: &(dyn Fn(Event) + Sync),
) -> anyhow::Result<Summary> {
    let Plan {
        start: Start { todo, scan },
        summary,
    } = plan;
    report(Event::Queued(todo.iter().map(|(p, _)| p.clone()).collect()));
    let mut collector = Collector {
        store,
        scan,
        opts,
        report,
        summary,
        total: todo.len(),
        done: 0,
        answers: HashMap::new(),
        waiting: Vec::new(),
    };
    thread::scope(|scope| -> anyhow::Result<()> {
        // Created inside the scope so an early return drops the receivers: workers and the
        // reader then fail their next send and stop, instead of running the rest of the queue.
        let (queue_tx, queue_rx) = mpsc::sync_channel::<Queued>(opts.jobs.max(1) * 2);
        let queue_rx = Arc::new(Mutex::new(queue_rx));
        let (msg_tx, msg_rx) = mpsc::channel::<Msg>();
        for _ in 0..opts.jobs.max(1) {
            let queue_rx = Arc::clone(&queue_rx);
            let msg_tx = msg_tx.clone();
            scope.spawn(move || work(&opts.backend, &queue_rx, &msg_tx));
        }
        let todo = &todo;
        scope.spawn(move || read(todo, opts, &queue_tx, &msg_tx, report));
        collector.collect(&msg_rx, log)
    })?;
    Ok(collector.summary)
}

/// What `scan` says when it is done, line by line (each to stderr): of a new run, or with
/// `adding` of what it added to one (`--run`).
pub fn summary_lines(s: &Summary, adding: bool) -> Vec<String> {
    if adding {
        return added_lines(s);
    }
    if s.run.is_empty() {
        return vec![if s.files == 0 {
            "Nothing to check: no MKV files found".to_string()
        } else {
            let paths: Vec<String> = s
                .paths
                .iter()
                .map(|p| shell_quote(&p.to_string_lossy()))
                .collect();
            format!(
                "Nothing to check: {} unchanged (subcheck review {})",
                s.cached,
                paths.join(" ")
            )
        }];
    }
    let mut lines = vec![format!(
        "run {}: {} files, {} checked, {} failed, {} unchanged since an earlier scan; {} findings{}",
        s.run,
        s.files,
        s.checked,
        s.failed,
        s.cached,
        s.findings,
        of_timing(s.timing)
    )];
    lines.extend(s.spent.line(", "));
    lines.push(review_hint(&s.run));
    lines
}

/// What `scan --run` says it did.
fn added_lines(s: &Summary) -> Vec<String> {
    let mut left = Vec::new();
    if s.cached > 0 {
        left.push(format!("{} with every check already", files(s.cached)));
    }
    if s.checked + s.failed == 0 {
        return vec![format!(
            "Nothing to add to run {}: {}",
            s.run,
            left.join(", ")
        )];
    }
    let left = if left.is_empty() {
        String::new()
    } else {
        format!(" ({} left out)", left.join(", "))
    };
    let mut lines = vec![format!(
        "run {}: added to {}{left}, {} failed; {} findings{}",
        s.run,
        files(s.checked + s.failed),
        s.failed,
        s.findings,
        of_timing(s.timing)
    )];
    lines.extend(s.spent.line(", "));
    lines.push(review_hint(&s.run));
    lines
}

/// ` (3 timing)`: how many of the findings are the timing check's; nothing when none are.
fn of_timing(n: usize) -> String {
    if n > 0 {
        format!(" ({n} timing)")
    } else {
        String::new()
    }
}

fn review_hint(run: &str) -> String {
    format!("Review them with: subcheck review {run} (or print them: subcheck list {run})")
}

/// Saves each file the moment every one of its lines has an answer.
struct Collector<'a> {
    store: &'a mut Store,
    scan: ScanRow,
    opts: &'a Options,
    /// Told of each file's progress and of every record saved.
    report: &'a (dyn Fn(Event) + Sync),
    summary: Summary,
    total: usize,
    done: usize,
    answers: HashMap<usize, Result<Vec<Suggestion>, String>>,
    waiting: Vec<FileJob>,
}

impl Collector<'_> {
    /// Runs until the reader and every worker have hung up.
    fn collect(&mut self, rx: &mpsc::Receiver<Msg>, log: &mut dyn Write) -> anyhow::Result<()> {
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::File(job) => {
                    let (answered, chunks) = self.asked(&job.refs);
                    if chunks > 0 {
                        (self.report)(Event::Asking {
                            path: job.prepared.path.clone(),
                            answered,
                            chunks,
                        });
                    }
                    self.waiting.push(*job);
                }
                Msg::Chunk(c) => {
                    // Saved as they come, so a scan cut short still says what it used.
                    if !c.calls.is_empty() {
                        for call in &c.calls {
                            self.summary.spent.add(call.usage.as_ref());
                        }
                        self.store.add_usage(self.scan.id, &c.calls)?;
                        (self.report)(Event::Appended(
                            c.calls.into_iter().map(Record::Usage).collect(),
                        ));
                    }
                    let id = c.id;
                    self.answers.insert(id, c.result);
                    for job in self
                        .waiting
                        .iter()
                        .filter(|j| j.refs.iter().any(|&(chunk, _)| chunk == id))
                    {
                        let (answered, chunks) = self.asked(&job.refs);
                        (self.report)(Event::Asking {
                            path: job.prepared.path.clone(),
                            answered,
                            chunks,
                        });
                    }
                }
            }
            while let Some(i) = self.waiting.iter().position(|j| {
                j.refs
                    .iter()
                    .all(|(chunk, _)| self.answers.contains_key(chunk))
            }) {
                let job = self.waiting.remove(i);
                let done = self.finish(job, log)?;
                (self.report)(done);
            }
        }
        for job in std::mem::take(&mut self.waiting) {
            let p = job.prepared;
            let mut said = Vec::new();
            if let Some(t) = p.truncated {
                say(log, &mut said, t.warning(&p.path))?;
            }
            self.summary.failed += 1;
            let error = "no answer for some lines".to_string();
            let path = p.path.to_string_lossy().into_owned();
            self.failed(&p, &path, &error)?;
            say(
                log,
                &mut said,
                format!("{}: FAILED: {error}", p.path.display()),
            )?;
            (self.report)(Event::Finished {
                path: p.path,
                error: Some(error),
                findings: 0,
                lines: said,
            });
        }
        Ok(())
    }

    /// A file whose lines all have their answers: saved in one transaction (its checks, or its
    /// failed attempt), its lines logged, and what it came to, for the hook. A save the database
    /// stays too busy for fails the file: nothing of it is saved, so the next scan checks it again.
    #[allow(clippy::too_many_lines)] // one file's save, then its log, in the order written
    fn finish(&mut self, job: FileJob, log: &mut dyn Write) -> anyhow::Result<Event> {
        let FileJob {
            prepared: mut p,
            refs,
        } = job;
        self.done += 1;
        let name = p
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let progress = format!("[{}/{}] {name}", self.done, self.total);
        let path = p.path.to_string_lossy().into_owned();
        let mut said = Vec::new();
        if let Some(t) = p.truncated {
            say(log, &mut said, t.warning(&p.path))?;
        }
        let llm_error = refs
            .iter()
            .find_map(|(chunk, _)| self.answers[chunk].as_ref().err())
            .map(|e| format!("LLM: {e}"));
        let mut error = p.error.clone().or(llm_error);
        let timing = p.timing.len();
        let mut found = 0;
        if let Some(e) = &error {
            self.failed(&p, &path, e)?;
        } else {
            let mut made = Vec::new();
            for &check in &p.checks {
                let findings = match check {
                    Check::Llm => findings(&self.answers, &p.lines, &refs, &path),
                    Check::Timing => std::mem::take(&mut p.timing),
                };
                made.push(Made {
                    with: checked_with(self.opts, check),
                    lines: p.lines.len(),
                    findings,
                });
            }
            match self.store.save(&self.attempt(&p, &path), made) {
                Ok(Saved { findings, carried }) => {
                    found = findings.len();
                    let mut records = vec![Record::File(record(&p, &path, None))];
                    records.extend(findings.into_iter().map(Record::Finding));
                    records.extend(carried.into_iter().map(Record::Decision));
                    (self.report)(Event::Appended(records));
                }
                Err(e) if store::is_busy(&e) => error = Some(format!("not saved: {e}")),
                Err(e) => return Err(e.into()),
            }
        }
        if let Some(e) = &error {
            self.summary.failed += 1;
            say(log, &mut said, format!("{progress}: FAILED: {e}"))?;
        } else {
            self.summary.checked += 1;
            self.summary.findings += found;
            self.summary.timing += timing;
            let line = if p.checks.contains(&Check::Llm) {
                format!(
                    "{progress}: {} lines, {found} findings{}",
                    p.lines.len(),
                    of_timing(timing)
                )
            } else {
                format!("{progress}: timing only, {found} findings")
            };
            say(log, &mut said, line)?;
        }
        for note in &p.timing_notes {
            say(log, &mut said, format!("    {note}"))?;
        }
        if self.opts.verbose {
            for note in &p.notes {
                say(log, &mut said, format!("    {note}"))?;
            }
        }
        Ok(Event::Finished {
            path: p.path,
            error,
            findings: found,
            lines: said,
        })
    }
}

impl Collector<'_> {
    /// What the scan read of the file, for its save.
    fn attempt<'p>(&self, p: &'p Prepared, path: &'p str) -> Attempt<'p> {
        Attempt {
            scan: self.scan.id,
            path,
            size: p.size,
            mtime: p.mtime,
            tracks: &p.tracks,
            truncated: p.truncated,
            at: Timestamp::now(),
        }
    }

    /// Save a failed attempt at the file's checks, and tell the hook. It replaces nothing: the
    /// file's earlier findings stay current. A database too busy to take it is no further error,
    /// as the file has failed already.
    fn failed(&mut self, p: &Prepared, path: &str, error: &str) -> anyhow::Result<()> {
        let tried: Vec<(CheckedWith, usize)> = p
            .checks
            .iter()
            .map(|&c| (checked_with(self.opts, c), p.lines.len()))
            .collect();
        match self.store.fail(&self.attempt(p, path), &tried, error) {
            Ok(()) => {
                let failed = record(p, path, Some(error));
                (self.report)(Event::Appended(vec![Record::File(failed)]));
                Ok(())
            }
            Err(e) if store::is_busy(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Of the chunks that answer a file's lines (`refs`), how many have answered, and how many
    /// there are.
    fn asked(&self, refs: &[(usize, usize)]) -> (usize, usize) {
        let mut ids: Vec<usize> = refs.iter().map(|&(chunk, _)| chunk).collect();
        ids.sort_unstable();
        ids.dedup();
        let answered = ids
            .iter()
            .filter(|id| self.answers.contains_key(id))
            .count();
        (answered, ids.len())
    }
}

/// The file's record of what this scan made of it, for the hook: the checks it made, or none for a
/// failed attempt, which makes none. Review's model merges it into the file's record as a view
/// would ([`FileRecord::merge`]).
fn record(p: &Prepared, path: &str, error: Option<&str>) -> FileRecord {
    FileRecord {
        path: path.to_string(),
        size: p.size,
        mtime: p.mtime,
        error: error.map(Into::into),
        tracks: p.tracks.clone(),
        lines: p.lines.len(),
        truncated: p.truncated,
        checks: if error.is_some() {
            Vec::new()
        } else {
            p.checks.clone()
        },
    }
}

/// Write `line` to the log, and keep it for the file's [`Event::Finished`].
fn say(log: &mut dyn Write, said: &mut Vec<String>, line: String) -> io::Result<()> {
    writeln!(log, "{line}")?;
    said.push(line);
    Ok(())
}

/// One finding per distinct suggestion for each line, with up to two lines of context either side.
/// Numbered 0: the store gives each its ID as it saves it.
fn findings(
    answers: &HashMap<usize, Result<Vec<Suggestion>, String>>,
    lines: &[Line],
    refs: &[(usize, usize)],
    path: &str,
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
                id: 0,
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

    fn run_with(backend: &str, langs: &[&str]) -> ScanRow {
        ScanRow {
            id: 1,
            name: "r".into(),
            header: RunHeader {
                started: Timestamp::UNIX_EPOCH,
                backend: backend.into(),
                command: Vec::new(),
                prompt_version: llm::PROMPT_VERSION,
                version: String::new(),
                langs: langs.iter().map(|l| (*l).to_string()).collect(),
            },
        }
    }

    fn langs(l: &[&str]) -> Vec<String> {
        l.iter().map(|l| (*l).to_string()).collect()
    }

    #[test]
    fn adding_to_a_run_takes_its_backend_and_languages_unless_they_are_given_alike() {
        let resume =
            |run: &ScanRow, llm, llm_cmd, langs: &[String]| super::resume(run, llm, llm_cmd, langs);
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
        // A backend this subcheck does not know: no command, refused later if a file needs it,
        // whatever command the run recorded.
        let mut gone = run_with("gemini", &["eng"]);
        gone.header.command = vec!["gemini".into(), "-p".into()];
        let (b, _) = resume(&gone, None, None, &[]).unwrap();
        assert_eq!((b.label.as_str(), b.argv.len()), ("gemini", 0));
    }

    /// A file as `finish` gets it when only the timing check is made of it and found nothing.
    fn timing_only(path: &str) -> FileJob {
        FileJob {
            prepared: Prepared {
                path: PathBuf::from(path),
                size: 10,
                mtime: Timestamp::UNIX_EPOCH,
                checks: vec![Check::Timing],
                tracks: Vec::new(),
                lines: Vec::new(),
                timing: Vec::new(),
                timing_notes: Vec::new(),
                error: None,
                truncated: None,
                notes: Vec::new(),
            },
            refs: Vec::new(),
        }
    }

    #[test]
    fn a_save_that_stays_busy_fails_that_file_alone_and_the_scan_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open_at(&dir.path().join("subcheck.db")).unwrap();
        store
            .set_busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        let files = [PathBuf::from("/m/a.mkv"), PathBuf::from("/m/b.mkv")];
        let header = run_with("claude", &["eng"]).header;
        let scan = store.begin_scan(&header, &files).unwrap();
        let opts = Options {
            backend: Backend::shell("true"),
            chunk_lines: 10,
            jobs: 1,
            langs: langs(&["eng"]),
            force: false,
            verbose: false,
            into: None,
        };
        let events = Mutex::new(Vec::new());
        let report = |e: Event| events.lock().unwrap().push(e);
        let mut collector = Collector {
            store: &mut store,
            scan,
            opts: &opts,
            report: &report,
            summary: Summary::default(),
            total: 2,
            done: 0,
            answers: HashMap::new(),
            waiting: Vec::new(),
        };
        let mut log = Vec::new();

        // Another connection holds the write lock while the first file is saved.
        let other = rusqlite::Connection::open(dir.path().join("subcheck.db")).unwrap();
        other.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let first = collector.finish(timing_only("/m/a.mkv"), &mut log).unwrap();
        other.execute_batch("ROLLBACK").unwrap();
        let Event::Finished {
            error, findings, ..
        } = first
        else {
            panic!("not a Finished event");
        };
        assert!(error.unwrap().starts_with("not saved: "));
        assert_eq!(findings, 0);

        // The lock is gone: the next file is saved.
        collector.finish(timing_only("/m/b.mkv"), &mut log).unwrap();
        assert_eq!(
            (collector.summary.failed, collector.summary.checked),
            (1, 1)
        );
        let log = String::from_utf8(log).unwrap();
        assert!(log.contains("[1/2] a.mkv: FAILED: not saved: "), "{log}");
        assert!(
            log.contains("[2/2] b.mkv: timing only, 0 findings"),
            "{log}"
        );

        let timing = checked_with(&opts, Check::Timing);
        let mtime = Timestamp::UNIX_EPOCH;
        assert!(!store.is_fresh("/m/a.mkv", 10, mtime, &timing).unwrap());
        assert!(store.is_fresh("/m/b.mkv", 10, mtime, &timing).unwrap());
        let rows: i64 = other
            .query_row(
                "SELECT COUNT(*) FROM checks WHERE path = '/m/a.mkv'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "a busy save leaves no attempt behind either");
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
        if rustix::process::geteuid().is_root() {
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

    #[test]
    fn mkv_files_given_a_path_with_dot_or_dotdot_finds_the_same_files() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("x")).unwrap();
        std::fs::create_dir_all(d.join("Show/Specials")).unwrap();
        for f in ["Show/a.mkv", "Show/Specials/b.mkv"] {
            std::fs::write(d.join(f), "").unwrap();
        }
        let plain = mkv_files(&[d.join("Show")]).unwrap().0;
        assert_eq!(plain, [d.join("Show/Specials/b.mkv"), d.join("Show/a.mkv")]);
        for odd in [d.join("x/../Show"), d.join("Show/.")] {
            assert_eq!(mkv_files(&[odd]).unwrap().0, plain);
        }
        // A file named through `..` is recorded at its clean path.
        let file = mkv_files(&[d.join("x/../Show/a.mkv")]).unwrap().0;
        assert_eq!(file, [d.join("Show/a.mkv")]);
    }
}
