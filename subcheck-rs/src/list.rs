//! `subcheck list`: a run's findings as plain text, grouped by file. The audit trail you can read,
//! grep or paste, with a ready mpv command for each finding on request.

use std::collections::HashMap;
use std::fmt::Write;

use crate::mkv::Truncated;
use crate::store::{FileRecord, Finding, Run, State, TrackRecord};
use crate::text::{Occurrence, timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Only(State),
}

/// mpv starts this far before a line, so you see it appear.
const LEAD_MS: i64 = 2000;

pub fn render(run: &Run, filter: Filter, mpv: bool) -> String {
    let mut out = String::new();
    let mut shown: Vec<&Finding> = run
        .findings
        .iter()
        .filter(|f| filter == Filter::All || filter == Filter::Only(run.state_of(f.id)))
        .collect();
    // A file's findings together, files in the order they first come: `scan --run` appends a
    // file's timing findings after every file's typos.
    let mut first: HashMap<&str, usize> = HashMap::new();
    for f in &shown {
        let n = first.len();
        first.entry(f.file.as_str()).or_insert(n);
    }
    shown.sort_by_key(|f| first[f.file.as_str()]);
    let failed: Vec<&FileRecord> = run.files.iter().filter(|f| f.error.is_some()).collect();
    let backend = run.header.as_ref().map_or("?", |h| h.backend.as_str());
    let _ = writeln!(
        out,
        "Run {} ({backend}): {} files, {} findings{}, {} failed",
        run.name,
        run.files.len(),
        shown.len(),
        match filter {
            Filter::All => String::new(),
            Filter::Only(state) => format!(" {}", label(state)),
        },
        failed.len()
    );
    let mut current = "";
    for f in &shown {
        if f.file != current {
            current = &f.file;
            let _ = writeln!(out, "\n{current}");
        }
        let first = f.occurrences.first();
        let when = first.map_or(String::new(), |o| timestamp(o.start_ms));
        let more = match f.occurrences.len() {
            0 | 1 => String::new(),
            n => format!(" (+{} more)", n - 1),
        };
        let state = run.state_of(f.id);
        let applied = if run.applied.contains(&f.id) {
            ", applied"
        } else {
            ""
        };
        let at = match &f.timing {
            Some(t) => track_label(run, f, t.track),
            None => format!("{when}{more}  {}", f.style),
        };
        let _ = writeln!(out, "  #{}  {at}  [{}{applied}]", f.id, label(state));
        let _ = writeln!(out, "      - {}", f.original);
        let edited = run.decisions.get(&f.id).and_then(|d| d.edited.as_deref());
        let _ = writeln!(out, "      + {}", edited.unwrap_or(&f.suggestion));
        if !f.reason.is_empty() {
            let _ = writeln!(out, "      why: {}", f.reason);
        }
        for l in f.timing.iter().flat_map(|t| &t.unshifted) {
            let _ = writeln!(
                out,
                "      not shifted: {} – {}",
                timestamp(l.start_ms),
                timestamp(l.end_ms)
            );
        }
        if mpv && f.timing.is_some() {
            for command in timing_mpv_commands(run, f) {
                let _ = writeln!(out, "      {command}");
            }
        } else if mpv {
            let _ = writeln!(out, "      {}", mpv_command(run, f));
        }
    }
    if shown.is_empty() {
        let _ = writeln!(out, "\nNo findings.");
    }
    if !failed.is_empty() {
        let _ = writeln!(out, "\nFailed (retried on the next scan):");
        for f in failed {
            let _ = writeln!(out, "  {}: {}", f.path, f.error.as_deref().unwrap_or(""));
        }
    }
    let truncated: Vec<(&str, Truncated)> = run
        .files
        .iter()
        .filter_map(|f| Some((f.path.as_str(), f.truncated?)))
        .collect();
    if !truncated.is_empty() {
        let _ = writeln!(out, "\nTruncated (replace these copies):");
        for (path, t) in truncated {
            let _ = writeln!(out, "  {path}: {}", t.shortfall());
        }
    }
    out
}

/// A state's name, as `list` and `review` show it.
pub fn label(state: State) -> &'static str {
    match state {
        State::Pending => "pending",
        State::Accepted => "accepted",
        State::Rejected => "rejected",
    }
}

/// Where a finding is shown: its occurrence on the file's default subtitle track if there is one,
/// else its first, with the track it is on.
pub fn shown_at<'a>(
    run: &'a Run,
    f: &'a Finding,
) -> (Option<&'a Occurrence>, Option<&'a TrackRecord>) {
    let tracks = run
        .files
        .iter()
        .rev()
        .find(|r| r.path == f.file)
        .map(|r| r.tracks.as_slice())
        .unwrap_or_default();
    let on_default = f
        .occurrences
        .iter()
        .find(|o| tracks.iter().any(|t| t.id == o.track && t.default));
    let occurrence = on_default.or(f.occurrences.first());
    let track = occurrence.and_then(|o| tracks.iter().find(|t| t.id == o.track));
    (occurrence, track)
}

/// Where mpv opens a finding: two seconds before its line (in ms), and mpv's sid for its track.
pub fn mpv_moment(run: &Run, f: &Finding) -> (i64, Option<usize>) {
    let (occurrence, track) = shown_at(run, f);
    (
        occurrence.map_or(0, |o| (o.start_ms - LEAD_MS).max(0)),
        track.map(|t| t.sid),
    )
}

/// The mpv command that opens the file two seconds before the line, with the subtitle track that
/// shows it: the occurrence on a default track if there is one, else the first.
pub fn mpv_command(run: &Run, f: &Finding) -> String {
    let (start, sid) = mpv_moment(run, f);
    let sid = sid.map(|s| format!(" --sid={s}")).unwrap_or_default();
    format!(
        "mpv --start={}{sid} {}",
        timestamp(start),
        shell_quote(&f.file)
    )
}

/// The track a timing finding is on: "track 5 English Full [Semisu]", or "track 5 (eng)" when it
/// has no name.
fn track_label(run: &Run, f: &Finding, id: usize) -> String {
    match track_of(run, f, id) {
        Some(TrackRecord {
            name: Some(name), ..
        }) => format!("track {id} {name}"),
        Some(t) => format!("track {id} ({})", t.language),
        None => format!("track {id}"),
    }
}

fn track_of<'a>(run: &'a Run, f: &Finding, id: usize) -> Option<&'a TrackRecord> {
    run.files
        .iter()
        .find(|r| r.path == f.file)
        .and_then(|r| r.tracks.iter().find(|t| t.id == id))
}

/// For a timing finding, an mpv command per shifted stretch that opens the file paused on the
/// frame showing the error, with the track's subtitles.
pub fn timing_mpv_commands(run: &Run, f: &Finding) -> Vec<String> {
    let Some(t) = &f.timing else {
        return Vec::new();
    };
    let sid = track_of(run, f, t.track)
        .map(|t| format!(" --sid={}", t.sid))
        .unwrap_or_default();
    t.stretches
        .iter()
        .filter_map(|s| s.frame_ms)
        .map(|ms| {
            format!(
                "mpv --start={} --pause{sid} {}",
                to_the_ms(ms),
                shell_quote(&f.file)
            )
        })
        .collect()
}

/// `H:MM:SS.mmm`: a frame's time to the millisecond, as mpv's `--start` takes it.
fn to_the_ms(ms: i64) -> String {
    format!(
        "{}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// Single-quoted for a POSIX shell (fish reads it the same way unless it holds a `\`). Always
/// quoted: even a `!` in a word with nothing else special is history to an interactive bash.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Check, Decision};

    fn run() -> Run {
        let track = |id, sid, default| TrackRecord {
            id,
            name: None,
            language: "eng".into(),
            sid,
            default,
            read: "cues".into(),
        };
        let file = FileRecord {
            path: "/m/K-On!! - E02.mkv".into(),
            size: 1,
            mtime: jiff::Timestamp::UNIX_EPOCH,
            error: None,
            tracks: vec![track(5, 1, true), track(6, 2, false)],
            lines: 486,
            truncated: None,
            checks: vec![Check::Llm],
        };
        let path = file.path.clone();
        let finding = |id, original: &str, suggestion: &str, occ: Vec<(usize, i64)>| Finding {
            id,
            file: path.clone(),
            style: "OP en".into(),
            original: original.into(),
            suggestion: suggestion.into(),
            reason: "typo".into(),
            source: "llm".into(),
            occurrences: occ
                .into_iter()
                .map(|(track, start_ms)| Occurrence {
                    track,
                    start_ms,
                    end_ms: start_ms + 1000,
                })
                .collect(),
            before: vec![],
            after: vec![],
            timing: None,
        };
        let mut run = Run {
            name: "2026-10-17T14-03-22".into(),
            files: vec![file],
            findings: vec![
                finding(
                    1,
                    "gets trough",
                    "gets through",
                    vec![(6, 165_060), (5, 165_060)],
                ),
                finding(2, "befor it", "before it", vec![(5, 779_080)]),
            ],
            ..Run::default()
        };
        run.decisions.insert(
            2,
            Decision {
                finding: 2,
                state: State::Accepted,
                edited: Some("Before it".into()),
                at: jiff::Timestamp::UNIX_EPOCH,
            },
        );
        run
    }

    #[test]
    fn findings_show_original_suggestion_and_state() {
        let text = render(&run(), Filter::All, false);
        assert!(text.contains("\n/m/K-On!! - E02.mkv\n  #1  0:02:45.06 (+1 more)  OP en  [pending]\n      - gets trough\n      + gets through\n      why: typo\n"), "{text}");
        assert!(
            text.contains(
                "  #2  0:12:59.08  OP en  [accepted]\n      - befor it\n      + Before it\n"
            ),
            "{text}"
        );
        assert!(!text.contains("mpv "));
    }

    #[test]
    fn an_applied_fix_says_so() {
        let mut r = run();
        r.applied.insert(2);
        let text = render(&r, Filter::All, false);
        assert!(
            text.contains("  #2  0:12:59.08  OP en  [accepted, applied]\n"),
            "{text}"
        );
        assert!(
            text.contains("  #1  0:02:45.06 (+1 more)  OP en  [pending]\n"),
            "{text}"
        );
    }

    #[test]
    fn filters_by_decision_state() {
        let text = render(&run(), Filter::Only(State::Pending), false);
        assert!(text.contains("#1 ") && !text.contains("#2 "), "{text}");
        let text = render(&run(), Filter::Only(State::Rejected), false);
        assert!(text.contains("No findings."), "{text}");
    }

    #[test]
    fn mpv_uses_the_default_track_and_starts_early() {
        let r = run();
        // Finding 1 is on tracks 6 and 5; 5 is the default one, mpv's sid 1.
        assert_eq!(
            mpv_command(&r, &r.findings[0]),
            "mpv --start=0:02:43.06 --sid=1 '/m/K-On!! - E02.mkv'"
        );
    }

    #[test]
    fn truncated_files_are_listed_for_replacing() {
        let mut r = run();
        assert!(!render(&r, Filter::All, false).contains("Truncated"));
        r.files[0].truncated = Some(Truncated {
            declared_end: 2000,
            file_size: 1000,
        });
        let text = render(&r, Filter::All, false);
        assert!(
            text.ends_with("\nTruncated (replace these copies):\n  /m/K-On!! - E02.mkv: file has 1000 of the 2000 bytes its Segment declares (50.0%)\n"),
            "{text}"
        );
    }

    #[test]
    fn a_timing_finding_shows_its_track_and_opens_on_each_frame_showing_its_error() {
        use crate::store::{Basis, FixStretch, LineRef, TimingFix};
        let mut r = run();
        r.files[0].tracks[0].name = Some("English Full [Semisu]".into());
        let line = |start_ms: i64| LineRef {
            raw: "0,D,,0,0,0,,Line.".into(),
            start_ms,
            end_ms: start_ms + 1000,
        };
        let stretch = |from_ms, to_ms, frames, frame_ms| FixStretch {
            from_ms,
            to_ms,
            frames,
            first: (frames != 0).then(|| line(0)),
            last: (frames != 0).then(|| line(0)),
            frame_ms,
        };
        r.findings = vec![Finding {
            id: 7,
            style: String::new(),
            original: "partial: 1 frame late, in sync, 1 frame late".into(),
            suggestion: "shift 1 frame earlier: start – 0:07:55.78, 0:15:40.47 – end".into(),
            reason: "cuts: 250 times, the offset changing 2 times".into(),
            source: "timing".into(),
            occurrences: vec![],
            timing: Some(TimingFix {
                track: 5,
                frame_ns: 41_708_333,
                stretches: vec![
                    stretch(None, Some(475_780), 1, Some(240_073)),
                    stretch(Some(483_510), Some(939_000), 0, None),
                    stretch(Some(940_470), None, 1, Some(1_200_115)),
                ],
                unshifted: vec![line(1_300_000)],
                basis: Basis::Cuts {
                    times: 250,
                    agreeing: 0,
                    windows: 9,
                },
            }),
            ..r.findings[0].clone()
        }];
        let text = render(&r, Filter::All, true);
        assert!(
            text.contains(
                "  #7  track 5 English Full [Semisu]  [pending]
      - partial: 1 frame late, in sync, 1 frame late
      + shift 1 frame earlier: start – 0:07:55.78, 0:15:40.47 – end
      why: cuts: 250 times, the offset changing 2 times
      not shifted: 0:21:40.00 – 0:21:41.00
      mpv --start=0:04:00.073 --pause --sid=1 '/m/K-On!! - E02.mkv'
      mpv --start=0:20:00.115 --pause --sid=1 '/m/K-On!! - E02.mkv'
"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_files_findings_are_listed_together_whatever_their_order_in_the_run() {
        let mut r = run();
        let other = Finding {
            id: 3,
            file: "/m/K-On!! - E03.mkv".into(),
            ..r.findings[1].clone()
        };
        let late = Finding {
            id: 4,
            ..r.findings[1].clone()
        };
        r.findings = vec![r.findings[0].clone(), other, late];
        let text = render(&r, Filter::All, false);
        let headers: Vec<&str> = text.lines().filter(|l| l.starts_with('/')).collect();
        assert_eq!(headers, ["/m/K-On!! - E02.mkv", "/m/K-On!! - E03.mkv"]);
        let ids: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("  #"))
            .map(|l| &l[..1])
            .collect();
        assert_eq!(ids, ["1", "4", "3"]);
    }

    #[test]
    fn quotes_survive_shell_quoting() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn a_history_bang_is_quoted() {
        // `!3,4` bare would be history entry 3 in an interactive bash.
        assert_eq!(shell_quote("!3,4"), "'!3,4'");
        assert_eq!(shell_quote("/m/K-On!!/E01.mkv"), "'/m/K-On!!/E01.mkv'");
    }
}
