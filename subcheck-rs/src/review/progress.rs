//! What review's screen shows of a scan and of apply: each file's stage and log, and each job's
//! counts. Pure: `update` feeds it the jobs' events. Opened on a run with no scan (`review`), it
//! holds the run's files as recorded.

use std::path::{Path, PathBuf};

use crate::apply::{self, patch::Outcome};
use crate::list::shell_quote;
use crate::llm::Spent;
use crate::scan::{self, Event};
use crate::store::{Check, FileRecord, Record, Run};

/// The spinner beside a stage under way, a step per tick.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Where a file is in the scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Waiting for the reader.
    Queued,
    /// Queued by `c` while a scan runs: checked again when it ends.
    QueuedAgain,
    Reading,
    /// Read; `answered` of the `chunks` its lines need have come back from the LLM.
    Asking {
        answered: usize,
        chunks: usize,
    },
    Done {
        findings: usize,
    },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProgress {
    pub path: String,
    pub stage: Stage,
    /// Distinct lines checked, from its record.
    pub lines: Option<usize>,
    /// Checked for timing alone, with no LLM check to count lines for: by the scan, as the record
    /// of what it made says, though the file's LLM check from an earlier scan stands; or, with no
    /// scan, as the file's record says.
    pub timing_only: bool,
    /// The scan checks it: a file of the run that the scan leaves alone is as recorded.
    pub queued: bool,
    /// What the scan wrote to its log for it; for a file the screen did not scan, what its record
    /// says.
    pub log: Vec<String>,
    /// Where the screen's last apply or dry run is with it; `None` for a file it had no fixes in.
    /// It takes the place of the scan's stage in the label.
    pub applying: Option<ApplyStage>,
    /// What each apply and dry run of the screen wrote for it, oldest first.
    pub apply_log: Vec<String>,
    /// mkvmerge's warnings for it, until its block comes: they go after it.
    pub warned: Vec<String>,
}

impl FileProgress {
    /// Where the file is, in a few words: `queued`, `queued again`, `⠙ LLM 2/3`,
    /// `✓ 412 lines · 3 findings`, `✓ timing only · 1 finding`, `✗ FAILED: …`. `spinner` turns
    /// beside a stage under way. Once apply has reached the file, its stage instead: queued,
    /// ⠙ remux, ✓ swapped · 3 applied.
    pub fn label(&self, spinner: char) -> String {
        if let Some(stage) = &self.applying {
            return stage.label(spinner);
        }
        match &self.stage {
            Stage::Queued => "queued".into(),
            Stage::QueuedAgain => "queued again".into(),
            Stage::Reading => format!("{spinner} reading"),
            Stage::Asking { answered, chunks } => format!("{spinner} LLM {answered}/{chunks}"),
            Stage::Done { findings } if self.timing_only => {
                format!("✓ timing only · {}", findings_text(*findings))
            }
            Stage::Done { findings } => match self.lines {
                Some(lines) => format!("✓ {lines} lines · {}", findings_text(*findings)),
                None => format!("✓ {}", findings_text(*findings)),
            },
            Stage::Failed(e) => format!("✗ FAILED: {e}"),
        }
    }
}

/// Whether a scan runs, and how it ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Scan {
    /// None was started: `review`.
    None,
    Running,
    Ended(Result<scan::Summary, String>),
}

/// What apply said of fixes (a file's, or all it did), counted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Made; for a dry run, to make.
    pub applied: usize,
    pub already: usize,
    pub by_hand: usize,
    pub not_found: usize,
}

impl Counts {
    /// Each fix's outcome, counted.
    pub fn of<'o>(outcomes: impl IntoIterator<Item = &'o Outcome>) -> Counts {
        let mut c = Counts::default();
        for o in outcomes {
            match o {
                Outcome::Patched | Outcome::Respaced { .. } | Outcome::Shifted { .. } => {
                    c.applied += 1;
                }
                Outcome::AlreadyApplied => c.already += 1,
                Outcome::ByHand(_) => c.by_hand += 1,
                Outcome::NotFound => c.not_found += 1,
            }
        }
        c
    }

    /// Add `other`'s counts to these.
    fn add(&mut self, other: Counts) {
        self.applied += other.applied;
        self.already += other.already;
        self.by_hand += other.by_hand;
        self.not_found += other.not_found;
    }

    /// The counts that are not 0, in words: `3 applied` (for a dry run `would apply 3`),
    /// `2 already`, `1 by hand`, `1 not found`.
    pub fn parts(&self, dry_run: bool) -> Vec<String> {
        let mut parts = Vec::new();
        if self.applied > 0 {
            parts.push(if dry_run {
                format!("would apply {}", self.applied)
            } else {
                format!("{} applied", self.applied)
            });
        }
        for (n, what) in [
            (self.already, "already"),
            (self.by_hand, "by hand"),
            (self.not_found, "not found"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {what}"));
            }
        }
        parts
    }
}

/// Where a file is in the screen's apply or dry run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyStage {
    /// It has fixes, and apply has not reached it.
    Queued,
    At(apply::Stage),
    Done {
        dry_run: bool,
        remuxed: bool,
        counts: Counts,
    },
    Failed(String),
}

impl ApplyStage {
    /// The stage in a few words, as `FileProgress::label` gives it.
    fn label(&self, spinner: char) -> String {
        match self {
            ApplyStage::Queued => "queued".into(),
            ApplyStage::At(stage) => format!("{spinner} {}", stage.word()),
            ApplyStage::Done {
                dry_run: false,
                remuxed,
                counts,
            } => {
                let head = if *remuxed { "swapped" } else { "not remuxed" };
                let mut parts = vec![head.to_string()];
                parts.extend(counts.parts(false));
                format!("✓ {}", parts.join(" · "))
            }
            ApplyStage::Done {
                dry_run: true,
                counts,
                ..
            } => {
                let mut parts = Vec::new();
                if counts.applied == 0 {
                    parts.push("nothing to apply".to_string());
                }
                parts.extend(counts.parts(true));
                format!("✓ {}", parts.join(" · "))
            }
            ApplyStage::Failed(e) => format!("✗ failed: {e}"),
        }
    }
}

/// The screen's apply, or a dry run: how far it is, and how it ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Applying {
    pub dry_run: bool,
    /// The files with fixes, those done, and of those the ones that failed and those remuxed.
    pub queued: usize,
    pub done: usize,
    pub failed: usize,
    pub remuxed: usize,
    /// The outcomes of the files done, counted.
    pub counts: Counts,
    /// `None` while it runs; then its summary, or why it failed as a whole.
    pub ended: Option<Result<apply::Summary, String>>,
    /// The command that makes the rest should it stop: `subcheck apply <view>`, with its flags.
    pub resume: String,
    /// `<path>: failed: <error>` for each file that failed, in order. Kept here, not read off the
    /// files view, because a scan that checks a file again replaces its progress, and the outro
    /// must still say it failed.
    pub failures: Vec<String>,
}

impl Applying {
    /// How the title and the status line start while it runs: `applying` or `dry run`.
    fn verb(&self) -> &'static str {
        if self.dry_run { "dry run" } else { "applying" }
    }

    /// What the quit question and the outro call it: `apply` or `dry run`.
    fn noun(&self) -> &'static str {
        if self.dry_run { "dry run" } else { "apply" }
    }

    /// What the files done so far came to, as `apply::run` sums them.
    fn so_far(&self) -> apply::Summary {
        apply::Summary {
            fixed: self.counts.applied,
            already: self.counts.already,
            by_hand: self.counts.by_hand,
            not_found: self.counts.not_found,
            remuxed: self.remuxed,
            failed: self.failed,
        }
    }

    /// The status line's text: how far it is while it runs, then its summary line, or why it
    /// failed.
    fn line(&self) -> String {
        match &self.ended {
            None => {
                let mut parts = vec![format!("{} {}/{}", self.verb(), self.done, self.queued)];
                parts.extend(self.counts.parts(self.dry_run));
                if self.failed > 0 {
                    parts.push(format!("{} failed", self.failed));
                }
                parts.join(" · ")
            }
            Some(Ok(s)) => apply::summary_line(s, !self.dry_run),
            Some(Err(e)) => format!("apply failed: {e}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // each says a different thing the outro or status line needs
pub struct Progress {
    /// The run's files, then any new ones the scan reads, in order.
    pub files: Vec<FileProgress>,
    pub scan: Scan,
    /// The scan adds to a run (`--run`): its summary is worded so.
    pub adding: bool,
    /// Of the files, those the scan checks, those done, and of those the ones that failed.
    pub queued: usize,
    pub done: usize,
    pub failed: usize,
    /// The findings the scan found.
    pub findings: usize,
    /// What the scan's LLM calls used.
    pub spent: Spent,
    /// Quarter seconds the scan has run: the spinner's step.
    pub ticks: usize,
    /// The run the scan records under, and the command that finishes it should it stop: for the
    /// outro. Empty before the first scan.
    pub run: String,
    pub resume: String,
    /// What the screen's earlier scans ended with, for the outro, oldest first: `c` starts another
    /// once one has ended.
    pub earlier: Vec<String>,
    /// An earlier scan of this screen failed, a file in it or the scan itself: `start` resets the
    /// counts, but the exit status still tells.
    pub earlier_failed: bool,
    /// Files `c` queued while a scan ran, each with the run it is checked into, in the order
    /// queued: they start when the scan ends, a scan for each run.
    pub again: Vec<(String, String)>,
    /// The screen's apply or dry run, as it runs or as the last ended; `None` before the first.
    pub apply: Option<Applying>,
    /// What the screen's earlier applies and dry runs ended with, for the outro, oldest first.
    pub earlier_applies: Vec<String>,
    /// An earlier apply or dry run of this screen failed, a file in it or as a whole.
    pub earlier_apply_failed: bool,
    /// Apply started after the last scan did, so the status line speaks of it.
    apply_latest: bool,
}

impl Progress {
    /// The run's files as recorded, and no scan.
    pub fn of_run(run: &Run) -> Progress {
        let files = run
            .files
            .iter()
            .map(|r| {
                let findings = run.findings.iter().filter(|f| f.file == r.path).count();
                FileProgress {
                    path: r.path.clone(),
                    stage: match &r.error {
                        Some(e) => Stage::Failed(e.clone()),
                        None => Stage::Done { findings },
                    },
                    lines: r.error.is_none().then_some(r.lines),
                    timing_only: r.error.is_none() && !r.checks.contains(&Check::Llm),
                    queued: false,
                    log: record_log(r, findings),
                    applying: None,
                    apply_log: Vec::new(),
                    warned: Vec::new(),
                }
            })
            .collect();
        Progress {
            files,
            scan: Scan::None,
            adding: false,
            queued: 0,
            done: 0,
            failed: 0,
            findings: 0,
            spent: Spent::default(),
            ticks: 0,
            run: String::new(),
            resume: String::new(),
            earlier: Vec::new(),
            earlier_failed: false,
            again: Vec::new(),
            apply: None,
            earlier_applies: Vec::new(),
            earlier_apply_failed: false,
            apply_latest: false,
        }
    }

    /// Take a step of the scan.
    pub fn scan_event(&mut self, event: &Event) {
        match event {
            Event::Queued(paths) => {
                self.scan = Scan::Running;
                self.queued = paths.len();
                for p in paths {
                    let path = p.to_string_lossy().into_owned();
                    let old = self.files.iter().position(|f| f.path == path);
                    let fresh = FileProgress {
                        path,
                        stage: Stage::Queued,
                        lines: None,
                        timing_only: false,
                        queued: true,
                        log: Vec::new(),
                        applying: None,
                        apply_log: Vec::new(),
                        warned: Vec::new(),
                    };
                    match old {
                        Some(i) => self.files[i] = fresh,
                        None => self.files.push(fresh),
                    }
                }
            }
            Event::Reading(path) => self.set(path, Stage::Reading),
            Event::Asking {
                path,
                answered,
                chunks,
            } => self.set(
                path,
                Stage::Asking {
                    answered: *answered,
                    chunks: *chunks,
                },
            ),
            Event::Appended(records) => {
                for r in records {
                    match r {
                        Record::Usage(call) => self.spent.add(call.usage.as_ref()),
                        Record::File(f) => {
                            if let Some(file) = self.file_mut(&f.path) {
                                file.lines = Some(f.lines);
                                file.timing_only = !f.checks.contains(&Check::Llm);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Finished {
                path,
                error,
                findings,
                lines,
            } => {
                self.done += 1;
                self.findings += findings;
                let stage = match error {
                    Some(e) => {
                        self.failed += 1;
                        Stage::Failed(e.clone())
                    }
                    None => Stage::Done {
                        findings: *findings,
                    },
                };
                if let Some(f) = self.file_mut(&path.to_string_lossy()) {
                    f.stage = stage;
                    f.log.clone_from(lines);
                }
            }
        }
    }

    /// A scan into run `run` from now: running, with nothing counted, and none of the files known
    /// to be its own until it says (`Event::Queued`). `resume` is the command that finishes it
    /// should it stop; `adding`, whether it adds to a run (`--run`). What the last scan ended with
    /// is kept for the outro.
    pub fn start(&mut self, run: &str, resume: String, adding: bool) {
        debug_assert!(
            !self.busy(),
            "a scan is started only once no job runs, or the last one's outro would be lost"
        );
        if matches!(self.scan, Scan::Ended(_)) {
            self.earlier = self.ended();
        }
        self.earlier_failed |= self.failed > 0 || matches!(self.scan, Scan::Ended(Err(_)));
        self.scan = Scan::Running;
        self.apply_latest = false;
        self.run = run.to_string();
        self.resume = resume;
        self.adding = adding;
        self.queued = 0;
        self.done = 0;
        self.failed = 0;
        self.findings = 0;
        self.spent = Spent::default();
        for f in &mut self.files {
            f.queued = false;
        }
    }

    /// The files of another view (`v`), and no job: what the screen's scans and applies ended with
    /// is kept, for the outro and the exit status.
    pub fn show(&mut self, run: &Run) {
        debug_assert!(!self.busy(), "`v` waits for the jobs to end");
        let earlier = self.ended();
        let earlier_failed =
            self.earlier_failed || self.failed > 0 || matches!(self.scan, Scan::Ended(Err(_)));
        let mut earlier_applies = std::mem::take(&mut self.earlier_applies);
        earlier_applies.extend(self.applied_lines());
        let earlier_apply_failed = self.apply_failed();
        *self = Progress {
            earlier,
            earlier_failed,
            earlier_applies,
            earlier_apply_failed,
            ..Progress::of_run(run)
        };
    }

    /// Apply (with `dry_run`, a dry run) from now: running, nothing counted, no file reached.
    /// `resume` is the command that makes the rest should it stop. What the last ended with is kept
    /// for the outro and the exit status.
    pub fn start_apply(&mut self, dry_run: bool, resume: String) {
        debug_assert!(!self.busy(), "apply starts only once no job runs");
        self.earlier_applies.extend(self.applied_lines());
        self.earlier_apply_failed = self.apply_failed();
        for f in &mut self.files {
            f.applying = None;
        }
        self.apply = Some(Applying {
            dry_run,
            queued: 0,
            done: 0,
            failed: 0,
            remuxed: 0,
            counts: Counts::default(),
            ended: None,
            resume,
            failures: Vec::new(),
        });
        self.apply_latest = true;
    }

    /// Take a step of apply.
    pub fn apply_event(&mut self, event: &apply::Event) {
        match event {
            apply::Event::Queued(paths) => {
                if let Some(a) = &mut self.apply {
                    a.queued = paths.len();
                }
                for p in paths {
                    self.set_applying(p, ApplyStage::Queued);
                }
            }
            apply::Event::File(_) => {}
            apply::Event::Stage(path, stage) => self.set_applying(path, ApplyStage::At(*stage)),
            apply::Event::Warned { path, text } => {
                if let Some(f) = self.file_mut(path) {
                    f.warned.push(text.clone());
                }
            }
            apply::Event::Done {
                path,
                outcomes,
                remuxed,
                text,
                ..
            } => {
                let counts = Counts::of(outcomes.iter().map(|(_, o)| o));
                let mut dry_run = false;
                if let Some(a) = &mut self.apply {
                    a.done += 1;
                    a.remuxed += usize::from(*remuxed);
                    a.counts.add(counts);
                    dry_run = a.dry_run;
                }
                let stage = ApplyStage::Done {
                    dry_run,
                    remuxed: *remuxed,
                    counts,
                };
                self.finish_applying(path, stage, text);
            }
            apply::Event::Failed { path, error, text } => {
                if let Some(a) = &mut self.apply {
                    a.done += 1;
                    a.failed += 1;
                    a.failures.push(format!("{path}: failed: {error}"));
                }
                self.finish_applying(path, ApplyStage::Failed(error.clone()), text);
            }
        }
    }

    /// Apply ended: its summary, or why it failed as a whole.
    pub fn apply_ended(&mut self, result: Result<apply::Summary, String>) {
        if let Some(a) = &mut self.apply {
            if let Err(e) = &result {
                for f in &mut self.files {
                    if matches!(f.applying, Some(ApplyStage::At(_))) {
                        f.applying = Some(ApplyStage::Failed(e.clone()));
                    }
                }
            }
            a.ended = Some(result);
        }
    }

    /// Whether apply or a dry run runs.
    pub fn applying(&self) -> bool {
        self.apply.as_ref().is_some_and(|a| a.ended.is_none())
    }

    /// Whether a job runs, a scan or apply: the loop ticks, `q` asks, and quitting stops it.
    pub fn busy(&self) -> bool {
        self.running() || self.applying()
    }

    /// Whether an apply or dry run of this screen failed, a file in it or as a whole: the exit
    /// status says so.
    pub fn apply_failed(&self) -> bool {
        self.earlier_apply_failed
            || self
                .apply
                .as_ref()
                .is_some_and(|a| a.failed > 0 || matches!(a.ended, Some(Err(_))))
    }

    /// What `q` asks while apply runs: the file it is on, by `label`'s name for it, and the stage;
    /// how far it is.
    pub fn apply_question(&self, label: &dyn Fn(&str) -> String) -> String {
        let Some(a) = &self.apply else {
            return String::new();
        };
        let at = match self.apply_at() {
            Some((path, stage)) => format!(" on {} ({}),", label(path), stage.word()),
            None => " on".into(),
        };
        format!(
            "{} is{at} {}/{} — stop it and quit? y/n",
            a.noun(),
            a.done,
            a.queued
        )
    }

    /// What the terminal gets of the screen's applies on leaving, after [`Progress::outro`]'s:
    /// what the earlier ones ended with, then the last (see `applied_lines`). One stopped says the
    /// file it was on (by `label`'s name) and its stage, what the files done came to, and the
    /// command that makes the rest.
    pub fn apply_outro(&self, label: &dyn Fn(&str) -> String) -> Vec<String> {
        let mut lines = self.earlier_applies.clone();
        let Some(a) = &self.apply else {
            return lines;
        };
        if a.ended.is_some() {
            lines.extend(self.applied_lines());
            return lines;
        }
        lines.extend(self.apply_failures());
        let at = match self.apply_at() {
            Some((path, stage)) => format!(" on {} ({})", label(path), stage.word()),
            None => String::new(),
        };
        lines.push(format!(
            "{} stopped{at}, {}/{} files done:",
            a.noun(),
            a.done,
            a.queued
        ));
        lines.push(apply::summary_line(&a.so_far(), !a.dry_run));
        lines.push(format!("Finish it with: {}", a.resume));
        lines
    }

    /// What the last apply ended with, for the outro: each file that failed, then its summary line
    /// as `subcheck apply` prints it, or why it failed. Nothing while it runs, or with none.
    fn applied_lines(&self) -> Vec<String> {
        let Some(Applying {
            ended: Some(ended),
            dry_run,
            ..
        }) = &self.apply
        else {
            return Vec::new();
        };
        let mut lines = self.apply_failures();
        lines.push(match ended {
            Ok(s) => apply::summary_line(s, !dry_run),
            Err(e) => format!("apply failed: {e}"),
        });
        lines
    }

    /// `<path>: failed: <error>` for each file the last apply failed.
    fn apply_failures(&self) -> Vec<String> {
        self.apply
            .as_ref()
            .map(|a| a.failures.clone())
            .unwrap_or_default()
    }

    /// The file apply is on, and its stage.
    fn apply_at(&self) -> Option<(&str, apply::Stage)> {
        self.files.iter().find_map(|f| match f.applying {
            Some(ApplyStage::At(stage)) => Some((f.path.as_str(), stage)),
            _ => None,
        })
    }

    /// Put file `path` at `stage` of apply.
    fn set_applying(&mut self, path: &str, stage: ApplyStage) {
        if let Some(f) = self.file_mut(path) {
            f.applying = Some(stage);
        }
    }

    /// File `path` is done, or failed: its stage, then its block in its log, then what mkvmerge
    /// warned of while it was remuxed.
    fn finish_applying(&mut self, path: &str, stage: ApplyStage, text: &str) {
        if let Some(f) = self.file_mut(path) {
            f.applying = Some(stage);
            f.apply_log.extend(text.lines().map(str::to_string));
            f.apply_log
                .extend(f.warned.drain(..).map(|w| format!("mkvmerge warned: {w}")));
        }
    }

    pub fn running(&self) -> bool {
        self.scan == Scan::Running
    }

    pub fn spinner(&self) -> char {
        SPINNER[self.ticks % SPINNER.len()]
    }

    /// While the scan runs, where it is with `path`, in the files view's words: `queued`,
    /// `reading`, `LLM 1/3`. `None` once the scan has ended, even if it left the file queued or
    /// being read, and for a file it is not checking or is done with.
    pub fn checking(&self, path: &str) -> Option<String> {
        if !self.running() {
            return None;
        }
        match &self.files.iter().find(|f| f.path == path)?.stage {
            Stage::Queued => Some("queued".into()),
            Stage::Reading => Some("reading".into()),
            Stage::Asking { answered, chunks } => Some(format!("LLM {answered}/{chunks}")),
            _ => None,
        }
    }

    /// `c` queued `path` to be checked again into run `run` when the scan ends.
    pub fn queue_again(&mut self, path: &str, run: &str) {
        if let Some(f) = self.file_mut(path) {
            f.stage = Stage::QueuedAgain;
        }
        self.again.push((path.to_string(), run.to_string()));
    }

    /// The files `c` queued into the first run among them, taken off the queue as one batch,
    /// with that run; the rest stay queued. `None` with nothing queued.
    pub fn next_batch(&mut self) -> Option<(String, Vec<String>)> {
        let run = self.again.first()?.1.clone();
        let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.again)
            .into_iter()
            .partition(|(_, r)| *r == run);
        self.again = later;
        Some((run, now.into_iter().map(|(p, _)| p).collect()))
    }

    /// `path` will not be checked, and why. It is not the earlier scan's any more, so that scan's
    /// outro does not list it among its failures.
    pub fn fail(&mut self, path: &str, why: &str) {
        if let Some(f) = self.file_mut(path) {
            f.stage = Stage::Failed(why.into());
            f.queued = false;
        }
    }

    /// For the list's title while a job runs: `scanning 7/24 ⠙`, `applying 3/9 ⠙`,
    /// `dry run 3/9 ⠙`.
    pub fn title(&self) -> Option<String> {
        if self.running() {
            return Some(format!(
                "scanning {}/{} {}",
                self.done,
                self.queued,
                self.spinner()
            ));
        }
        let a = self.apply.as_ref().filter(|a| a.ended.is_none())?;
        Some(format!(
            "{} {}/{} {}",
            a.verb(),
            a.done,
            a.queued,
            self.spinner()
        ))
    }

    /// The status line's own text: while the scan runs, how far it is (`scan 7/24 · 31 findings ·
    /// 1 failed · $0.84 at API prices`); once it ends, the first line of `scan`'s summary, or why
    /// it failed. `None` with no scan. Once apply has started (and no scan since), apply's instead
    /// (see `Applying::line`).
    pub fn line(&self) -> Option<String> {
        // Apply's, when it started after the last scan and no scan runs.
        if self.apply_latest
            && !self.running()
            && let Some(a) = &self.apply
        {
            return Some(a.line());
        }
        match &self.scan {
            Scan::None => None,
            Scan::Running => {
                let mut parts = vec![
                    format!("scan {}/{}", self.done, self.queued),
                    findings_text(self.findings),
                ];
                if self.failed > 0 {
                    parts.push(format!("{} failed", self.failed));
                }
                parts.extend(self.spent.price_part());
                Some(parts.join(" · "))
            }
            Scan::Ended(Ok(s)) => scan::summary_lines(s, self.adding).into_iter().next(),
            Scan::Ended(Err(e)) => Some(format!("the scan failed: {e}")),
        }
    }

    /// What `q` asks while the scan runs, with the files `c` queued counted: quitting drops them.
    pub fn quit_question(&self) -> String {
        let queued = match self.again.len() {
            0 => String::new(),
            n => format!(" · {n} queued to check again"),
        };
        format!(
            "scan is on {}/{}{queued} — stop it and quit? y/n",
            self.done, self.queued
        )
    }

    /// What the terminal gets on leaving the screen: what the scans ended with (see `ended`),
    /// then the command that checks the files `c` queued, for each run.
    pub fn outro(&self) -> Vec<String> {
        let mut lines = self.ended();
        lines.extend(self.not_checked_again());
        lines
    }

    /// What the screen's scans ended with, for the outro and for the next scan to keep: of this
    /// one, each file it checked that failed (a run's earlier failures elsewhere are not this
    /// scan's, nor in its count), then what `scan` would have ended with; for a scan stopped or
    /// failed, how far it got and the command that finishes it. What earlier scans ended with
    /// comes first. Nothing with no scan.
    fn ended(&self) -> Vec<String> {
        let mut lines = self.earlier.clone();
        if self.scan == Scan::None {
            return lines;
        }
        lines.extend(
            self.files
                .iter()
                .filter(|f| f.queued)
                .filter_map(|f| match &f.stage {
                    Stage::Failed(e) => Some(format!("{}: FAILED: {e}", f.path)),
                    _ => None,
                }),
        );
        match &self.scan {
            Scan::Ended(Ok(s)) => lines.extend(scan::summary_lines(s, self.adding)),
            Scan::Ended(Err(e)) => {
                lines.push(format!("the scan failed: {e}"));
                lines.push(format!("Finish it with: {}", self.resume));
            }
            Scan::Running | Scan::None => {
                lines.push(format!(
                    "run {}: stopped at {}/{} files, {} failed; {}",
                    self.run,
                    self.done,
                    self.queued,
                    self.failed,
                    findings_text(self.findings)
                ));
                lines.extend(self.spent.line(", "));
                lines.push(format!("Finish it with: {}", self.resume));
            }
        }
        lines
    }

    /// For each run with files `c` queued, in the order first queued, the command that checks
    /// them. `Finish it with` does not cover them: without `--force`, `--run` checks only files
    /// the run has no good record of, and a file `c` queued has one, unless its last check
    /// failed.
    fn not_checked_again(&self) -> Vec<String> {
        let mut runs: Vec<&str> = Vec::new();
        for (_, run) in &self.again {
            if !runs.contains(&run.as_str()) {
                runs.push(run.as_str());
            }
        }
        runs.into_iter()
            .map(|run| {
                let paths: Vec<PathBuf> = self
                    .again
                    .iter()
                    .filter(|(_, r)| r == run)
                    .map(|(p, _)| PathBuf::from(p))
                    .collect();
                format!("Not checked again: {}", again_command(run, &paths))
            })
            .collect()
    }

    fn file_mut(&mut self, path: &str) -> Option<&mut FileProgress> {
        self.files.iter_mut().find(|f| f.path == path)
    }

    fn set(&mut self, path: &Path, stage: Stage) {
        if let Some(f) = self.file_mut(&path.to_string_lossy()) {
            f.stage = stage;
        }
    }
}

/// The command that finishes a scan stopped partway: `--run` checks every file the run has no good
/// record of. `paths` as the scan was given them.
pub fn resume_command(run: &str, paths: &[PathBuf]) -> String {
    format!("subcheck scan --run {run} {}", quoted(paths))
}

/// The command that checks `paths` again into run `run`: for a `c` stopped partway, or for files
/// `c` queued that never started.
pub fn again_command(run: &str, paths: &[PathBuf]) -> String {
    format!("subcheck scan --run {run} --force {}", quoted(paths))
}

/// `paths`, each quoted for the shell, with a space between.
fn quoted(paths: &[PathBuf]) -> String {
    let paths: Vec<String> = paths
        .iter()
        .map(|p| shell_quote(&p.to_string_lossy()))
        .collect();
    paths.join(" ")
}

/// "1 finding", "3 findings".
fn findings_text(n: usize) -> String {
    if n == 1 {
        "1 finding".into()
    } else {
        format!("{n} findings")
    }
}

/// What a file's record says, for the detail pane of a file the screen did not scan.
fn record_log(r: &FileRecord, findings: usize) -> Vec<String> {
    let mut log = vec![match &r.error {
        Some(e) => format!("FAILED: {e}"),
        None => format!("{} lines, {}", r.lines, findings_text(findings)),
    }];
    log.extend(
        r.tracks
            .iter()
            .map(|t| format!("    track {} {}: {}", t.id, t.shown_name(), t.read)),
    );
    if let Some(t) = r.truncated {
        log.push(format!("    scanned a partial copy: {}", t.shortfall()));
    }
    log
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::{self, Summary, patch::Outcome};
    use crate::store::{Check, FileRecord, Finding, LlmCall};

    fn record(path: &str, lines: usize, error: Option<&str>) -> FileRecord {
        FileRecord {
            path: path.into(),
            size: 1,
            mtime: jiff::Timestamp::UNIX_EPOCH,
            error: error.map(Into::into),
            tracks: vec![],
            lines,
            truncated: None,
            checks: vec![Check::Llm],
        }
    }

    fn queued() -> Progress {
        let mut p = Progress::of_run(&Run::default());
        p.scan_event(&Event::Queued(vec![
            "/m/E01.mkv".into(),
            "/m/E02.mkv".into(),
        ]));
        p
    }

    #[test]
    fn a_files_stage_follows_the_scan() {
        let mut p = queued();
        assert!(p.running());
        assert_eq!(p.files[0].label('⠙'), "queued");
        p.scan_event(&Event::Reading("/m/E01.mkv".into()));
        assert_eq!(p.files[0].label('⠙'), "⠙ reading");
        p.scan_event(&Event::Asking {
            path: "/m/E01.mkv".into(),
            answered: 1,
            chunks: 3,
        });
        assert_eq!(p.files[0].label('⠙'), "⠙ LLM 1/3");
        p.scan_event(&Event::Appended(vec![Record::File(record(
            "/m/E01.mkv",
            412,
            None,
        ))]));
        p.scan_event(&Event::Finished {
            path: "/m/E01.mkv".into(),
            error: None,
            findings: 3,
            lines: vec!["[1/2] E01.mkv: 412 lines, 3 findings".into()],
        });
        assert_eq!(p.files[0].label('⠙'), "✓ 412 lines · 3 findings");
        assert_eq!(p.files[0].log, ["[1/2] E01.mkv: 412 lines, 3 findings"]);
        p.scan_event(&Event::Finished {
            path: "/m/E02.mkv".into(),
            error: Some("LLM: boom".into()),
            findings: 0,
            lines: vec![],
        });
        assert_eq!(p.files[1].label('⠙'), "✗ FAILED: LLM: boom");
        assert_eq!((p.done, p.failed, p.findings), (2, 1, 3));
        assert_eq!(
            p.line().as_deref(),
            Some("scan 2/2 · 3 findings · 1 failed")
        );
        assert_eq!(p.title().as_deref(), Some("scanning 2/2 ⠋"));
        p.ticks += 1;
        assert_eq!(p.title().as_deref(), Some("scanning 2/2 ⠙"));
    }

    #[test]
    fn the_line_gives_the_price_while_the_scan_runs_and_its_summary_once_it_ends() {
        let mut p = queued();
        let usage = crate::llm::Usage {
            input_tokens: 1000,
            output_tokens: 10,
            cost_usd: Some(3.21),
            ..crate::llm::Usage::default()
        };
        p.scan_event(&Event::Appended(vec![Record::Usage(LlmCall {
            file: "/m/E01.mkv".into(),
            lines: 4,
            usage: Some(usage),
        })]));
        assert_eq!(
            p.line().as_deref(),
            Some("scan 0/2 · 0 findings · $3.21 at API prices")
        );
        p.scan = Scan::Ended(Ok(scan::Summary {
            run: "r".into(),
            files: 2,
            checked: 2,
            ..scan::Summary::default()
        }));
        assert_eq!(
            p.line().as_deref(),
            Some(
                "run r: 2 files, 2 checked, 0 failed, 0 unchanged since an earlier scan; 0 findings"
            )
        );
        assert_eq!(p.title(), None);
        p.scan = Scan::Ended(Err("disk full".into()));
        assert_eq!(p.line().as_deref(), Some("the scan failed: disk full"));
    }

    #[test]
    fn a_runs_files_show_as_recorded_and_a_scan_adding_to_it_queues_them_again() {
        let run = Run {
            files: vec![
                record("/m/E01.mkv", 400, None),
                record("/m/E02.mkv", 0, Some("cannot read")),
            ],
            findings: vec![Finding {
                id: 1,
                file: "/m/E01.mkv".into(),
                style: String::new(),
                original: "a".into(),
                suggestion: "b".into(),
                reason: String::new(),
                source: "llm".into(),
                occurrences: vec![],
                before: vec![],
                after: vec![],
                timing: None,
            }],
            ..Run::default()
        };
        let mut p = Progress::of_run(&run);
        assert!(!p.running());
        assert_eq!((p.line(), p.title()), (None, None));
        assert_eq!(p.files[0].label('⠙'), "✓ 400 lines · 1 finding");
        assert_eq!(p.files[1].label('⠙'), "✗ FAILED: cannot read");
        assert_eq!(p.files[0].log, ["400 lines, 1 finding"]);
        // `--run`: E02 checked again, E03 new; E01 left as it was.
        p.scan_event(&Event::Queued(vec![
            "/m/E02.mkv".into(),
            "/m/E03.mkv".into(),
        ]));
        let stages: Vec<&Stage> = p.files.iter().map(|f| &f.stage).collect();
        assert_eq!(
            stages,
            [&Stage::Done { findings: 1 }, &Stage::Queued, &Stage::Queued]
        );
        assert_eq!(p.queued, 2);
    }

    #[test]
    fn leaving_says_what_scan_would_have_or_how_to_finish_a_stopped_one() {
        let mut p = queued();
        p.scan_event(&Event::Finished {
            path: "/m/E01.mkv".into(),
            error: Some("LLM: boom".into()),
            findings: 0,
            lines: vec![],
        });
        let resume = resume_command("r", &["/m/K-On! S1".into()]);
        assert_eq!(resume, "subcheck scan --run r '/m/K-On! S1'");
        p.run = "r".into();
        p.resume = resume.clone();
        assert_eq!(
            p.outro(),
            [
                "/m/E01.mkv: FAILED: LLM: boom",
                "run r: stopped at 1/2 files, 1 failed; 0 findings",
                "Finish it with: subcheck scan --run r '/m/K-On! S1'",
            ]
        );
        p.scan = Scan::Ended(Ok(scan::Summary {
            run: "r".into(),
            files: 2,
            checked: 1,
            failed: 1,
            ..scan::Summary::default()
        }));
        p.run = "r".into();
        p.resume = resume.clone();
        assert_eq!(
            p.outro(),
            [
                "/m/E01.mkv: FAILED: LLM: boom",
                "run r: 2 files, 1 checked, 1 failed, 0 unchanged since an earlier scan; 0 findings",
                "Review them with: subcheck review r (or print them: subcheck list r)",
            ]
        );
        assert_eq!(
            Progress::of_run(&Run::default()).outro(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn leaving_lists_the_failures_of_the_files_the_scan_checked_alone() {
        // `--run` on a run whose E01, in another folder, failed before: this scan checks E02 alone,
        // which fails too.
        let run = Run {
            files: vec![record("/m/S1/E01.mkv", 0, Some("cannot read"))],
            ..Run::default()
        };
        let mut p = Progress::of_run(&run);
        p.scan_event(&Event::Queued(vec!["/m/S2/E02.mkv".into()]));
        p.scan_event(&Event::Finished {
            path: "/m/S2/E02.mkv".into(),
            error: Some("LLM: boom".into()),
            findings: 0,
            lines: vec![],
        });
        p.run = "r".into();
        p.resume = "subcheck scan --run r /m/S2".into();
        assert_eq!(
            p.outro(),
            [
                "/m/S2/E02.mkv: FAILED: LLM: boom",
                "run r: stopped at 1/1 files, 1 failed; 0 findings",
                "Finish it with: subcheck scan --run r /m/S2",
            ]
        );
    }

    #[test]
    fn a_file_checked_for_timing_alone_says_so_instead_of_its_lines() {
        let checked = |path, lines, checks: &[Check]| FileRecord {
            checks: checks.to_vec(),
            ..record(path, lines, None)
        };
        // E01's LLM check stands, so the scan makes the timing check alone; E03 failed before, so
        // it is checked whole; E02 is new to the run, its LLM check made by another scan.
        let run = Run {
            files: vec![
                record("/m/E01.mkv", 412, None),
                record("/m/E03.mkv", 0, Some("cannot read")),
            ],
            ..Run::default()
        };
        let mut p = Progress::of_run(&run);
        p.scan_event(&Event::Queued(vec![
            "/m/E01.mkv".into(),
            "/m/E02.mkv".into(),
            "/m/E03.mkv".into(),
        ]));
        let both = [Check::Llm, Check::Timing];
        for (path, lines, checks, findings) in [
            ("/m/E01.mkv", 0, [Check::Timing].as_slice(), 1),
            ("/m/E02.mkv", 0, &[Check::Timing], 2),
            ("/m/E03.mkv", 380, &both, 3),
        ] {
            p.scan_event(&Event::Appended(vec![Record::File(checked(
                path, lines, checks,
            ))]));
            p.scan_event(&Event::Finished {
                path: path.into(),
                error: None,
                findings,
                lines: vec![],
            });
        }
        let labels: Vec<(&str, String)> = p
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.label('⠙')))
            .collect();
        assert_eq!(
            labels,
            [
                ("/m/E01.mkv", "✓ timing only · 1 finding".to_string()),
                ("/m/E03.mkv", "✓ 380 lines · 3 findings".to_string()),
                ("/m/E02.mkv", "✓ timing only · 2 findings".to_string()),
            ]
        );
        // With no scan, a record of the timing check alone says so too.
        let p = Progress::of_run(&Run {
            files: vec![checked("/m/E02.mkv", 0, &[Check::Timing])],
            ..Run::default()
        });
        assert_eq!(p.files[0].label('⠙'), "✓ timing only · 0 findings");
    }

    #[test]
    fn a_file_is_being_checked_from_queued_until_it_is_done() {
        let mut p = Progress::of_run(&Run::default());
        p.scan_event(&Event::Queued(vec!["/m/E01.mkv".into()]));
        assert_eq!(p.checking("/m/E01.mkv").as_deref(), Some("queued"));
        p.scan_event(&Event::Reading("/m/E01.mkv".into()));
        assert_eq!(p.checking("/m/E01.mkv").as_deref(), Some("reading"));
        p.scan_event(&Event::Asking {
            path: "/m/E01.mkv".into(),
            answered: 1,
            chunks: 3,
        });
        assert_eq!(p.checking("/m/E01.mkv").as_deref(), Some("LLM 1/3"));
        p.scan_event(&Event::Finished {
            path: "/m/E01.mkv".into(),
            error: None,
            findings: 0,
            lines: vec![],
        });
        assert_eq!(p.checking("/m/E01.mkv"), None);
        assert_eq!(p.checking("/m/E02.mkv"), None);
    }

    #[test]
    fn a_file_left_queued_by_a_scan_that_failed_is_not_being_checked() {
        let mut p = Progress::of_run(&Run::default());
        p.scan_event(&Event::Queued(vec!["/m/E01.mkv".into()]));
        assert_eq!(p.checking("/m/E01.mkv").as_deref(), Some("queued"));
        p.scan = Scan::Ended(Err("disk full".into()));
        assert_eq!(p.checking("/m/E01.mkv"), None);
    }

    #[test]
    fn a_scan_started_after_one_ended_counts_afresh_and_the_outro_keeps_both() {
        let mut p = Progress::of_run(&Run::default());
        p.start("first", "subcheck scan --run first /m".into(), false);
        p.scan_event(&Event::Queued(vec!["/m/E01.mkv".into()]));
        p.scan_event(&Event::Finished {
            path: "/m/E01.mkv".into(),
            error: Some("LLM: boom".into()),
            findings: 0,
            lines: vec![],
        });
        p.scan = Scan::Ended(Err("disk full".into()));
        p.start(
            "second",
            "subcheck scan --run second --force /m/E01.mkv".into(),
            true,
        );
        assert!(p.running());
        assert_eq!((p.queued, p.done, p.failed, p.findings), (0, 0, 0, 0));
        assert!(p.adding);
        assert_eq!(
            p.outro(),
            [
                "/m/E01.mkv: FAILED: LLM: boom",
                "the scan failed: disk full",
                "Finish it with: subcheck scan --run first /m",
                "run second: stopped at 0/0 files, 0 failed; 0 findings",
                "Finish it with: subcheck scan --run second --force /m/E01.mkv",
            ]
        );
    }

    #[test]
    fn the_quit_question_counts_the_files_c_queued() {
        let mut p = Progress::of_run(&Run::default());
        p.start("r", "subcheck scan --run r /m".into(), false);
        p.scan_event(&Event::Queued(vec![
            "/m/E01.mkv".into(),
            "/m/E02.mkv".into(),
        ]));
        assert_eq!(p.quit_question(), "scan is on 0/2 — stop it and quit? y/n");
        p.queue_again("/m/E03.mkv", "r");
        p.queue_again("/m/E04.mkv", "s");
        assert_eq!(
            p.quit_question(),
            "scan is on 0/2 · 2 queued to check again — stop it and quit? y/n"
        );
    }

    #[test]
    fn the_outro_gives_the_command_for_each_runs_queued_files_and_none_for_a_batch_taken() {
        let mut p = Progress::of_run(&Run::default());
        p.start("r", "subcheck scan --run r /m".into(), false);
        p.scan_event(&Event::Queued(vec!["/m/E01.mkv".into()]));
        p.queue_again("/m/K-On! E03.mkv", "r");
        p.queue_again("/m/E04.mkv", "s");
        p.queue_again("/m/E05.mkv", "r");
        assert_eq!(
            p.outro(),
            [
                "run r: stopped at 0/1 files, 0 failed; 0 findings",
                "Finish it with: subcheck scan --run r /m",
                "Not checked again: subcheck scan --run r --force '/m/K-On! E03.mkv' '/m/E05.mkv'",
                "Not checked again: subcheck scan --run s --force '/m/E04.mkv'",
            ]
        );
        // r's files go as one batch, in the order queued; s's stay queued.
        assert_eq!(
            p.next_batch(),
            Some((
                "r".to_string(),
                vec!["/m/K-On! E03.mkv".to_string(), "/m/E05.mkv".to_string()]
            ))
        );
        p.scan = Scan::Ended(Ok(crate::scan::Summary::default()));
        p.start(
            "r",
            "subcheck scan --run r --force '/m/K-On! E03.mkv' '/m/E05.mkv'".into(),
            true,
        );
        // What the first scan ended with is kept without the queue's line, so s's is said once.
        let outro = p.outro();
        assert_eq!(
            outro.last().map(String::as_str),
            Some("Not checked again: subcheck scan --run s --force '/m/E04.mkv'")
        );
        assert_eq!(
            outro
                .iter()
                .filter(|l| l.starts_with("Not checked again"))
                .count(),
            1,
            "{outro:#?}"
        );
        assert_eq!(
            p.next_batch(),
            Some(("s".to_string(), vec!["/m/E04.mkv".to_string()]))
        );
        assert_eq!(p.next_batch(), None);
        assert!(!p.outro().iter().any(|l| l.starts_with("Not checked again")));
    }

    #[test]
    fn another_view_keeps_what_the_screens_scans_ended_with() {
        let mut p = Progress::of_run(&Run::default());
        p.start("r", "subcheck scan --run r /m".into(), false);
        p.scan_event(&Event::Queued(vec!["/m/E01.mkv".into()]));
        p.scan_event(&Event::Finished {
            path: "/m/E01.mkv".into(),
            error: Some("LLM: boom".into()),
            findings: 0,
            lines: vec![],
        });
        p.scan = Scan::Ended(Ok(crate::scan::Summary::default()));
        let outro = p.outro();
        p.show(&Run::default());
        assert_eq!(p.scan, Scan::None);
        assert_eq!(p.files.len(), 0);
        assert_eq!(p.outro(), outro);
        assert!(p.earlier_failed);
    }

    /// Two files of a run with an apply (or a dry run) started on them, both queued.
    fn applying(dry_run: bool) -> Progress {
        let mut p = Progress::of_run(&Run {
            files: vec![
                record("/m/E01.mkv", 400, None),
                record("/m/E02.mkv", 400, None),
            ],
            ..Run::default()
        });
        p.start_apply(dry_run, "subcheck apply r".into());
        p.apply_event(&apply::Event::Queued(vec![
            "/m/E01.mkv".into(),
            "/m/E02.mkv".into(),
        ]));
        p
    }

    /// File `path` done, with `outcomes`.
    fn done(path: &str, outcomes: Vec<(u64, Outcome)>, remuxed: bool) -> apply::Event {
        apply::Event::Done {
            path: path.into(),
            outcomes,
            remuxed,
            marked: vec![],
            restamped: None,
            text: format!("{path}\n  remuxed\n\n"),
        }
    }

    /// The name a test gives a file: `E01` for `/m/E01.mkv`.
    fn short(path: &str) -> String {
        path.trim_start_matches("/m/")
            .trim_end_matches(".mkv")
            .into()
    }

    #[test]
    fn a_files_label_follows_apply() {
        let mut p = applying(false);
        assert!(p.applying() && p.busy() && !p.running());
        assert_eq!(p.files[0].label('⠙'), "queued");
        p.apply_event(&apply::Event::Stage(
            "/m/E01.mkv".into(),
            apply::Stage::Remux,
        ));
        assert_eq!(p.files[0].label('⠙'), "⠙ remux");
        assert_eq!(p.title().as_deref(), Some("applying 0/2 ⠋"));
        p.apply_event(&apply::Event::Warned {
            path: "/m/E01.mkv".into(),
            text: "a warning".into(),
        });
        p.apply_event(&done(
            "/m/E01.mkv",
            vec![
                (1, Outcome::Patched),
                (2, Outcome::Patched),
                (3, Outcome::ByHand("a karaoke row".into())),
            ],
            true,
        ));
        assert_eq!(p.files[0].label('⠙'), "✓ swapped · 2 applied · 1 by hand");
        assert_eq!(
            p.files[0].apply_log,
            ["/m/E01.mkv", "  remuxed", "", "mkvmerge warned: a warning"]
        );
        assert_eq!(
            p.line().as_deref(),
            Some("applying 1/2 · 2 applied · 1 by hand")
        );
        p.apply_event(&done(
            "/m/E02.mkv",
            vec![(4, Outcome::AlreadyApplied)],
            false,
        ));
        assert_eq!(p.files[1].label('⠙'), "✓ not remuxed · 1 already");
        p.apply_ended(Ok(Summary {
            fixed: 2,
            already: 1,
            by_hand: 1,
            not_found: 0,
            remuxed: 1,
            failed: 0,
        }));
        assert!(!p.busy());
        assert_eq!(p.title(), None);
        assert_eq!(
            p.line().as_deref(),
            Some("applied 2, already applied 1, by hand 1, not found 0; 1 remuxed, 0 failed")
        );
        assert!(!p.apply_failed());
        assert_eq!(
            p.apply_outro(&short),
            ["applied 2, already applied 1, by hand 1, not found 0; 1 remuxed, 0 failed"]
        );
    }

    #[test]
    fn a_dry_runs_labels_say_what_would_happen() {
        let mut p = applying(true);
        p.apply_event(&apply::Event::Stage(
            "/m/E01.mkv".into(),
            apply::Stage::Plan,
        ));
        assert_eq!(p.title().as_deref(), Some("dry run 0/2 ⠋"));
        p.apply_event(&done(
            "/m/E01.mkv",
            vec![(1, Outcome::Patched), (2, Outcome::NotFound)],
            false,
        ));
        assert_eq!(p.files[0].label('⠙'), "✓ would apply 1 · 1 not found");
        p.apply_event(&done(
            "/m/E02.mkv",
            vec![(4, Outcome::AlreadyApplied)],
            false,
        ));
        assert_eq!(p.files[1].label('⠙'), "✓ nothing to apply · 1 already");
        assert_eq!(
            p.line().as_deref(),
            Some("dry run 2/2 · would apply 1 · 1 already · 1 not found")
        );
        p.apply_ended(Ok(Summary {
            fixed: 1,
            already: 1,
            not_found: 1,
            ..Summary::default()
        }));
        assert_eq!(
            p.line().as_deref(),
            Some("to apply 1, already applied 1, by hand 0, not found 1; 0 remuxed, 0 failed")
        );
    }

    #[test]
    fn a_stopped_apply_says_how_far_it_got_and_how_to_finish_it() {
        let mut p = applying(false);
        p.apply_event(&apply::Event::Failed {
            path: "/m/E01.mkv".into(),
            error: "disk full".into(),
            text: "/m/E01.mkv\n  failed: disk full\n\n".into(),
        });
        p.apply_event(&apply::Event::Stage(
            "/m/E02.mkv".into(),
            apply::Stage::Remux,
        ));
        assert_eq!(p.files[0].label('⠙'), "✗ failed: disk full");
        assert_eq!(
            p.apply_question(&short),
            "apply is on E02 (remux), 1/2 — stop it and quit? y/n"
        );
        assert_eq!(
            p.apply_outro(&short),
            [
                "/m/E01.mkv: failed: disk full",
                "apply stopped on E02 (remux), 1/2 files done:",
                "applied 0, already applied 0, by hand 0, not found 0; 0 remuxed, 1 failed",
                "Finish it with: subcheck apply r",
            ]
        );
        assert!(p.apply_failed());
    }

    #[test]
    fn the_outro_keeps_a_failed_file_though_a_scan_checked_it_again() {
        let mut p = applying(false);
        p.apply_event(&apply::Event::Failed {
            path: "/m/E01.mkv".into(),
            error: "disk full".into(),
            text: "/m/E01.mkv\n  failed: disk full\n\n".into(),
        });
        p.apply_event(&done("/m/E02.mkv", vec![(1, Outcome::Patched)], true));
        p.apply_ended(Ok(Summary {
            fixed: 1,
            remuxed: 1,
            failed: 1,
            ..Summary::default()
        }));
        p.start("r", "subcheck scan --run r /m".into(), true);
        p.scan_event(&Event::Queued(vec!["/m/E01.mkv".into()]));
        assert_eq!(p.files[0].applying, None);
        assert_eq!(
            p.apply_outro(&short)[0],
            "/m/E01.mkv: failed: disk full",
            "the failure lines do not depend on the files view"
        );
    }

    #[test]
    fn an_apply_that_fails_as_a_whole_leaves_no_file_mid_stage() {
        let mut p = applying(false);
        p.apply_event(&apply::Event::Stage(
            "/m/E01.mkv".into(),
            apply::Stage::Remux,
        ));
        p.apply_ended(Err("mkvmerge: killed".into()));
        assert_eq!(
            p.files[0].applying,
            Some(ApplyStage::Failed("mkvmerge: killed".into()))
        );
        assert_eq!(p.files[1].applying, Some(ApplyStage::Queued));
        assert_eq!(p.apply.as_ref().unwrap().failed, 0);
        assert_eq!(p.apply_outro(&short), ["apply failed: mkvmerge: killed"]);
    }

    #[test]
    fn earlier_applies_are_kept_for_the_outro_across_another_apply_and_another_view() {
        let mut p = applying(true);
        p.apply_ended(Err("mkvmerge: not found".into()));
        assert_eq!(
            p.line().as_deref(),
            Some("apply failed: mkvmerge: not found")
        );
        p.start_apply(false, "subcheck apply r".into());
        assert_eq!(p.files[0].applying, None);
        p.apply_event(&apply::Event::Queued(vec!["/m/E01.mkv".into()]));
        p.apply_event(&done("/m/E01.mkv", vec![(1, Outcome::Patched)], true));
        p.apply_ended(Ok(Summary {
            fixed: 1,
            remuxed: 1,
            ..Summary::default()
        }));
        p.show(&Run::default());
        assert_eq!(p.apply, None);
        assert_eq!(p.line(), None);
        assert_eq!(
            p.apply_outro(&short),
            [
                "apply failed: mkvmerge: not found",
                "applied 1, already applied 0, by hand 0, not found 0; 1 remuxed, 0 failed",
            ]
        );
        assert!(p.apply_failed(), "the dry run failed as a whole");
    }

    #[test]
    fn the_status_line_speaks_of_the_job_that_started_last() {
        let mut p = applying(false);
        p.apply_ended(Ok(Summary::default()));
        assert!(p.line().unwrap().starts_with("applied 0"));
        p.start("r", "subcheck scan --run r /m".into(), true);
        p.scan_event(&Event::Queued(vec!["/m/E01.mkv".into()]));
        assert!(p.line().unwrap().starts_with("scan 0/1"));
        p.scan = Scan::Ended(Err("disk full".into()));
        assert_eq!(p.line().as_deref(), Some("the scan failed: disk full"));
    }
}
