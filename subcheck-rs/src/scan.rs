//! `subcheck scan`: find MKVs, read their text subtitle tracks, send the lines to the LLM in
//! chunks, and record every suggested fix in a run file.
//!
//! One reader thread reads files in order (the array's disks are the bottleneck, so never two at
//! once), `-j` workers run the LLM, and the calling thread collects: it appends a file's records
//! the moment every one of that file's lines has an answer.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use jiff::Timestamp;

use crate::blocks::{self, ReadPath};
use crate::llm::{self, Backend, Suggestion};
use crate::mkv::{self, Track, Truncated, TRACK_TYPE_SUBTITLE};
use crate::store::{
    Cache, CacheEntry, CheckedWith, FileRecord, Finding, Record, RunHeader, RunWriter, TrackRecord,
};
use crate::subs::Kind;
use crate::text::{self, Line};

pub struct Options {
    pub backend: Backend,
    pub chunk_lines: usize,
    pub jobs: usize,
    /// ISO 639-2 codes; a track matches by `Language` or by its BCP 47 primary subtag.
    pub langs: Vec<String>,
    pub force: bool,
    pub verbose: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub run: String,
    pub files: usize,
    pub checked: usize,
    pub failed: usize,
    pub cached: usize,
    pub findings: usize,
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
                primary == want || iso639_1(want) == Some(primary)
            }
            None => track.language == *want,
        })
}

/// A file read and turned into lines, or the reason it could not be.
struct Prepared {
    path: PathBuf,
    size: u64,
    mtime: Timestamp,
    tracks: Vec<TrackRecord>,
    lines: Vec<Line>,
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

fn prepare(path: &Path, langs: &[String]) -> Prepared {
    let mut p = Prepared {
        path: path.to_path_buf(),
        size: 0,
        mtime: Timestamp::UNIX_EPOCH,
        tracks: Vec::new(),
        lines: Vec::new(),
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
        if let Some(kind) = Kind::of(track) {
            if wanted_language(track, langs) {
                wanted.push((track, kind, sid));
            }
        }
    }
    let numbers: Vec<u64> = wanted.iter().map(|(t, _, _)| t.number).collect();
    let before = r.bytes_read();
    let reads = blocks::read_each(&r, &layout, &numbers);
    let mut events = Vec::new();
    for ((track, kind, sid), read) in wanted.into_iter().zip(reads) {
        let read = match read {
            Ok(t) => {
                let path = match t.path {
                    ReadPath::Cues => "cues",
                    ReadPath::Walk => "walk",
                    ReadPath::Empty => "empty",
                };
                p.notes.push(format!(
                    "track {} {}: {} events via {path}",
                    track.id,
                    track.name.as_deref().unwrap_or(""),
                    t.blocks.len()
                ));
                events.extend(text::events(
                    track.id,
                    kind,
                    &t.blocks,
                    layout.timestamp_scale,
                    track.default_duration,
                ));
                path.to_string()
            }
            // An unsupported encoding loses that track, not the file.
            Err(e) => {
                p.notes.push(format!("track {}: skipped: {e}", track.id));
                p.skipped_tracks = true;
                format!("skipped: {e}")
            }
        };
        p.tracks.push(TrackRecord {
            id: track.id,
            name: track.name.clone(),
            language: track.language.clone(),
            sid,
            default: track.default,
            read,
        });
    }
    p.notes
        .push(format!("{} bytes read", r.bytes_read() - before));
    p.lines = text::lines(events);
    p
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

/// The files that need checking, and how many were left out as unchanged since an earlier scan
/// made with the same backend, languages and prompt.
fn to_check(
    all: Vec<PathBuf>,
    opts: &Options,
    state: &Path,
) -> anyhow::Result<(Vec<PathBuf>, usize)> {
    let cache = Cache::load(state)?;
    let with = checked_with(opts);
    let mut cached = 0;
    let todo = all
        .into_iter()
        .filter(|path| {
            let fresh = !opts.force
                && file_stamp(path).is_ok_and(|(size, mtime)| {
                    cache.is_fresh(&path.to_string_lossy(), size, mtime, &with)
                });
            cached += usize::from(fresh);
            !fresh
        })
        .collect();
    Ok((todo, cached))
}

fn checked_with(opts: &Options) -> CheckedWith {
    CheckedWith::new(&opts.backend.label, &opts.langs, llm::PROMPT_VERSION)
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
    let (todo, cached) = to_check(all, opts, state)?;
    if cached > 0 {
        writeln!(
            log,
            "dry run: {} unchanged since an earlier scan, left out (--force to include)",
            files(cached)
        )?;
    }
    let mut chunker = Chunker::new(opts.chunk_lines);
    for path in todo {
        let p = prepare(&path, &opts.langs);
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
fn read(
    todo: &[PathBuf],
    opts: &Options,
    queue: &mpsc::SyncSender<Chunk>,
    out: &mpsc::Sender<Msg>,
) {
    let mut chunker = Chunker::new(opts.chunk_lines);
    for path in todo {
        let prepared = prepare(path, &opts.langs);
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
    let (todo, cached) = to_check(all, opts, state)?;
    summary.cached = cached;
    // No run file for a scan with nothing to check: an empty run would become the newest and
    // hide the last real one from a plain `subcheck list`.
    if todo.is_empty() {
        return Ok(summary);
    }
    let mut writer = RunWriter::create(state)?;
    summary.run.clone_from(&writer.name);
    writer.append(&[Record::Run(RunHeader {
        started: Timestamp::now(),
        backend: opts.backend.label.clone(),
        command: opts.backend.argv.clone(),
        prompt_version: llm::PROMPT_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
    })])?;
    let mut collector = Collector {
        writer,
        state,
        opts,
        with: checked_with(opts),
        summary,
        total: todo.len(),
        done: 0,
        next_finding: 1,
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
    with: CheckedWith,
    summary: Summary,
    total: usize,
    done: usize,
    next_finding: u64,
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
            self.writer.append(&[Record::File(FileRecord {
                path: p.path.to_string_lossy().into_owned(),
                size: p.size,
                mtime: p.mtime,
                error: Some(error.clone()),
                tracks: p.tracks,
                lines: p.lines.len(),
                truncated: p.truncated,
            })])?;
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
        let file = Record::File(FileRecord {
            path: path.clone(),
            size: p.size,
            mtime: p.mtime,
            error: error.clone(),
            tracks: p.tracks,
            lines: p.lines.len(),
            truncated: p.truncated,
        });
        if let Some(e) = error {
            self.summary.failed += 1;
            self.writer.append(&[file])?;
            writeln!(log, "{progress}: FAILED: {e}")?;
        } else {
            let findings = findings(
                &self.answers,
                &p.lines,
                &refs,
                &path,
                &mut self.next_finding,
            );
            let found = findings.len();
            let mut records = vec![file];
            records.extend(findings.into_iter().map(Record::Finding));
            self.writer.append(&records)?;
            if !p.skipped_tracks && p.truncated.is_none() {
                Cache::append(
                    self.state,
                    &CacheEntry {
                        path,
                        size: p.size,
                        mtime: p.mtime,
                        with: self.with.clone(),
                        run: self.writer.name.clone(),
                    },
                )?;
            }
            self.summary.checked += 1;
            self.summary.findings += found;
            writeln!(log, "{progress}: {} lines, {found} findings", p.lines.len())?;
        }
        if self.opts.verbose {
            for note in &p.notes {
                writeln!(log, "    {note}")?;
            }
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
