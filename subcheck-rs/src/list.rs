//! `subcheck list`: a run's findings as plain text, grouped by file. The audit trail you can read,
//! grep or paste, with a ready mpv command for each finding on request.

use std::collections::HashMap;
use std::fmt::Write;

use crate::mkv::{self, Truncated};
use crate::store::{FileRecord, Finding, Run, State, TrackRecord};
use crate::text::{Occurrence, timestamp, timestamp_ms};

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
    let heading = if run.of_paths {
        run.name.clone()
    } else {
        format!("Run {} ({backend})", run.name)
    };
    let _ = writeln!(
        out,
        "{heading}: {} files, {} findings{}, {} failed",
        run.files.len(),
        shown.len(),
        match filter {
            Filter::All => String::new(),
            Filter::Only(state) => format!(" {}", label(state)),
        },
        failed.len()
    );
    if let Some(spent) = run.spent.line(", ") {
        let _ = writeln!(out, "{spent}");
    }
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
    match track_name_in(tracks_of(run, &f.file), id) {
        Some((name, _)) => format!("track {id} {name}"),
        None => format!("track {id}"),
    }
}

/// Track `id` among a file record's `tracks` by [name](TrackRecord::shown_name), and whether that
/// tells it apart, being a name of its own no other track of the file has. `None` when it is not
/// among them.
pub fn track_name_in(tracks: &[TrackRecord], id: usize) -> Option<(String, bool)> {
    let t = tracks.iter().find(|t| t.id == id)?;
    let own = mkv::own_name(t.name.as_deref());
    let apart = own.is_some_and(|own| {
        tracks
            .iter()
            .filter(|x| mkv::own_name(x.name.as_deref()) == Some(own))
            .count()
            == 1
    });
    Some((t.shown_name(), apart))
}

/// The tracks the run's record of file `path` lists; none when it has no record of it.
fn tracks_of<'a>(run: &'a Run, path: &str) -> &'a [TrackRecord] {
    run.files
        .iter()
        .find(|r| r.path == path)
        .map_or(&[], |r| r.tracks.as_slice())
}

/// Track `id` of a finding's file, as the run's record of the file has it.
pub fn track_of<'a>(run: &'a Run, f: &Finding, id: usize) -> Option<&'a TrackRecord> {
    tracks_of(run, &f.file).iter().find(|t| t.id == id)
}

/// Where review's `o` opens a timing finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingMoment {
    pub at_ms: i64,
    /// mpv's sid for its track.
    pub sid: Option<usize>,
    /// On the frame that shows the error, not only a line of a shifted stretch.
    pub on_frame: bool,
}

/// Where review's `o` opens a timing finding: the first frame that shows a shifted stretch's
/// error, else the first shifted stretch's first line. `None` for other findings.
pub fn timing_moment(run: &Run, f: &Finding) -> Option<TimingMoment> {
    let t = f.timing.as_ref()?;
    let shifted = || t.stretches.iter().filter(|s| s.frames != 0);
    let (at_ms, on_frame) = match shifted().find_map(|s| s.frame_ms) {
        Some(at) => (at, true),
        None => (shifted().find_map(|s| s.first.as_ref())?.start_ms, false),
    };
    Some(TimingMoment {
        at_ms,
        sid: track_of(run, f, t.track).map(|r| r.sid),
        on_frame,
    })
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
                timestamp_ms(ms),
                shell_quote(&f.file)
            )
        })
        .collect()
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
            language_bcp47: None,
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
    fn what_the_runs_llm_calls_cost_follows_its_line_when_they_were_recorded() {
        let mut r = run();
        // A run from before calls were recorded, or one whose LLM reports nothing.
        let text = render(&r, Filter::All, false);
        assert!(!text.contains("LLM"), "{text}");
        r.spent.add(Some(&crate::llm::Usage {
            input_tokens: 1_234_567,
            output_tokens: 31_234,
            cost_usd: Some(3.21),
            ..crate::llm::Usage::default()
        }));
        let text = render(&r, Filter::All, false);
        assert!(
            text.starts_with(
                "Run 2026-10-17T14-03-22 (?): 1 files, 2 findings, 0 failed\n\
                 LLM: 1 call, 1.23M in / 31.2k out tokens, $3.21 at API prices\n\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn the_files_under_some_paths_are_headed_by_the_paths() {
        let r = Run {
            name: "'/m/K-On!!'".into(),
            of_paths: true,
            ..run()
        };
        let text = render(&r, Filter::All, false);
        assert!(
            text.starts_with("'/m/K-On!!': 1 files, 2 findings, 0 failed\n"),
            "{text}"
        );
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
    fn a_timing_finding_opens_on_its_frame_else_its_first_moved_line() {
        let r = run();
        let mut f = crate::review::model::tests_support::timing(9, 2, true);
        f.file.clone_from(&r.files[0].path);
        let moment = |at_ms, on_frame| {
            Some(TimingMoment {
                at_ms,
                sid: Some(1),
                on_frame,
            })
        };
        // The first shifted stretch's frame, on track 5 (sid 1).
        assert_eq!(timing_moment(&r, &f), moment(246_788, true));
        // Without it, the next shifted stretch's frame.
        if let Some(t) = &mut f.timing {
            t.stretches[0].frame_ms = None;
        }
        assert_eq!(timing_moment(&r, &f), moment(1_196_153, true));
        // With no frame at all, the first shifted stretch's first line.
        if let Some(t) = &mut f.timing {
            t.stretches[2].frame_ms = None;
        }
        assert_eq!(timing_moment(&r, &f), moment(1000, false));
        assert_eq!(timing_moment(&r, &r.findings[0]), None);
    }

    #[test]
    fn an_empty_track_name_names_nothing() {
        let track = |id, name: Option<&str>| TrackRecord {
            id,
            name: name.map(Into::into),
            language: "eng".into(),
            language_bcp47: None,
            sid: id,
            default: false,
            read: "cues".into(),
        };
        let tracks = [
            track(5, Some("")),
            track(6, Some("  ")),
            track(7, Some("Signs")),
        ];
        assert_eq!(track_name_in(&tracks, 5), Some(("(eng)".into(), false)));
        assert_eq!(track_name_in(&tracks, 6), Some(("(eng)".into(), false)));
        assert_eq!(track_name_in(&tracks, 7), Some(("Signs".into(), true)));
    }

    #[test]
    fn an_unnamed_track_is_named_by_its_bcp_47_tag_first() {
        let track = |id, language: &str, bcp47: Option<&str>| TrackRecord {
            id,
            name: None,
            language: language.into(),
            language_bcp47: bcp47.map(Into::into),
            sid: id,
            default: false,
            read: "cues".into(),
        };
        // `eng` beside `ja` is a track tagged only in BCP 47: Language's default, as recorded.
        let tracks = [
            track(3, "jpn", Some("ja")),
            track(4, "eng", Some("ja")),
            track(5, "jpn", None),
        ];
        assert_eq!(track_name_in(&tracks, 3), Some(("(ja)".into(), false)));
        assert_eq!(track_name_in(&tracks, 4), Some(("(ja)".into(), false)));
        assert_eq!(track_name_in(&tracks, 5), Some(("(jpn)".into(), false)));
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
