//! What review's screen shows of a scan: each file's stage and log, and the scan's counts. Pure:
//! `update` feeds it the scan's events. Opened on a run with no scan (`review`), it holds the
//! run's files as recorded.

use std::path::{Path, PathBuf};

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
    /// Checked for timing alone, with no LLM check of its own to count lines for, as its record
    /// says. A record that adds the timing check to a run's (`--run`) keeps the old record's lines
    /// and its LLM check, which this scan did not make.
    pub timing_only: bool,
    /// The scan checks it: a file of the run that the scan leaves alone is as recorded.
    pub queued: bool,
    /// Its good record in the run, from before the scan, has the LLM check: adding to the run
    /// makes only the checks that record lacks, so the scan's record of it is of timing alone.
    had_llm: bool,
    /// What the scan wrote to its log for it; for a file the screen did not scan, what its record
    /// says.
    pub log: Vec<String>,
}

impl FileProgress {
    /// Where the file is, in a few words: `queued`, `⠙ LLM 2/3`, `✓ 412 lines · 3 findings`,
    /// `✓ timing only · 1 finding`, `✗ FAILED: …`. `spinner` turns beside a stage under way.
    pub fn label(&self, spinner: char) -> String {
        match &self.stage {
            Stage::Queued => "queued".into(),
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

#[derive(Debug, Clone, PartialEq)]
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
                    had_llm: r.error.is_none() && r.checks.contains(&Check::Llm),
                    log: record_log(r, findings),
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
                        had_llm: old.is_some_and(|i| self.files[i].had_llm),
                        log: Vec::new(),
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
                                file.timing_only = file.had_llm || !f.checks.contains(&Check::Llm);
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

    pub fn running(&self) -> bool {
        self.scan == Scan::Running
    }

    pub fn spinner(&self) -> char {
        SPINNER[self.ticks % SPINNER.len()]
    }

    /// For the list's title while the scan runs: `scanning 7/24 ⠙`.
    pub fn title(&self) -> Option<String> {
        self.running()
            .then(|| format!("scanning {}/{} {}", self.done, self.queued, self.spinner()))
    }

    /// The status line's own text: while the scan runs, how far it is (`scan 7/24 · 31 findings ·
    /// 1 failed · $0.84 at API prices`); once it ends, the first line of `scan`'s summary, or why
    /// it failed. `None` with no scan.
    pub fn line(&self) -> Option<String> {
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

    /// What `q` asks while the scan runs.
    pub fn quit_question(&self) -> String {
        format!(
            "scan is on {}/{} — stop it and quit? y/n",
            self.done, self.queued
        )
    }

    /// What the terminal gets on leaving the screen, of the scan: each file it checked that failed
    /// (a run's earlier failures elsewhere are not this scan's, nor in its count), then what `scan`
    /// would have ended with; for a scan stopped or failed, how far it got and `resume`, the
    /// command that finishes it. Nothing with no scan.
    pub fn outro(&self, run: &str, resume: &str) -> Vec<String> {
        if self.scan == Scan::None {
            return Vec::new();
        }
        let mut lines: Vec<String> = self
            .files
            .iter()
            .filter(|f| f.queued)
            .filter_map(|f| match &f.stage {
                Stage::Failed(e) => Some(format!("{}: FAILED: {e}", f.path)),
                _ => None,
            })
            .collect();
        match &self.scan {
            Scan::Ended(Ok(s)) => lines.extend(scan::summary_lines(s, self.adding)),
            Scan::Ended(Err(e)) => {
                lines.push(format!("the scan failed: {e}"));
                lines.push(format!("Finish it with: {resume}"));
            }
            Scan::Running | Scan::None => {
                lines.push(format!(
                    "run {run}: stopped at {}/{} files, {} failed; {}",
                    self.done,
                    self.queued,
                    self.failed,
                    findings_text(self.findings)
                ));
                lines.extend(self.spent.line(", "));
                lines.push(format!("Finish it with: {resume}"));
            }
        }
        lines
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
    let paths: Vec<String> = paths
        .iter()
        .map(|p| shell_quote(&p.to_string_lossy()))
        .collect();
    format!("subcheck scan --run {run} {}", paths.join(" "))
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
        assert_eq!(
            p.outro("r", &resume),
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
        assert_eq!(
            p.outro("r", &resume),
            [
                "/m/E01.mkv: FAILED: LLM: boom",
                "run r: 2 files, 1 checked, 1 failed, 0 unchanged since an earlier scan; 0 findings",
                "Review them with: subcheck review r (or print them: subcheck list r)",
            ]
        );
        assert_eq!(
            Progress::of_run(&Run::default()).outro("r", &resume),
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
        assert_eq!(
            p.outro("r", "subcheck scan --run r /m/S2"),
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
        // `--run`: E01's record has the LLM check, so the timing check is added alone (its new
        // record keeps the old one's lines); E03 failed before, so it is checked whole; E02 is new,
        // its LLM check cached from another scan.
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
            ("/m/E01.mkv", 412, both.as_slice(), 1),
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
}
