//! A file's timing findings, as a run records them: one for each track whose lines need moving,
//! by its own times against the cuts or by the lines it shares with another track.

use super::shared::Lines;
use super::{Analysis, Stretch, Verdict, Video, detail, frame_offset, nearest_keyframe};
use super::{frames_way, shift_label, verdict_label};
use crate::store::{Basis, Finding, FixStretch, LineRef, TimingFix};
use crate::text::{self, Event, Occurrence};

/// One track of a file, as read and analysed.
pub struct Track<'a> {
    /// mkvmerge ID.
    pub id: usize,
    pub events: &'a [Event],
    pub analysis: &'a Analysis,
    /// What its lines shared with another track say.
    pub lines: Option<&'a Lines>,
}

/// The file's timing findings, numbered 0 for the run to number, and a note for each track whose
/// own verdict its shared lines contradict (it gets no finding).
pub fn findings(file: &str, video: &Video, tracks: &[Track]) -> (Vec<Finding>, Vec<String>) {
    let mut out = Vec::new();
    let mut notes = Vec::new();
    for t in tracks {
        let a = t.analysis;
        let (verdict, each, basis) = match (&a.verdict, t.lines) {
            (_, Some(Lines::Disagrees { source, says })) => {
                notes.push(format!(
                    "track {}: its times against the cuts say {}, its lines shared with track \
                     {source} say {}: no fix",
                    t.id,
                    verdict_label(&a.verdict),
                    verdict_label(says)
                ));
                continue;
            }
            (Verdict::Shifted(_) | Verdict::Partial(_), _) => (
                &a.verdict,
                &a.each,
                Basis::Cuts {
                    times: a.times,
                    agreeing: a.agreeing,
                    windows: a.windows,
                },
            ),
            (_, Some(Lines::Follows(f)))
                if matches!(f.verdict, Verdict::Shifted(_) | Verdict::Partial(_)) =>
            {
                (
                    &f.verdict,
                    &f.each,
                    Basis::Lines {
                        source: f.source,
                        shared: f.shared.times,
                        at_gap: f.shared.at(f.gap),
                        gap: f.gap,
                    },
                )
            }
            _ => continue,
        };
        let stretches = match verdict {
            Verdict::Shifted(frames) => vec![Stretch {
                from_ms: None,
                to_ms: None,
                frames: *frames,
            }],
            Verdict::Partial(stretches) => stretches.clone(),
            _ => continue,
        };
        let mut occurrences = Vec::new();
        let fixed: Vec<FixStretch> = stretches
            .iter()
            .enumerate()
            .map(|(k, s)| {
                let (fixed, shown) = fix_stretch(video, t.events, each, k, s);
                occurrences.extend(shown.map(|e| Occurrence {
                    track: t.id,
                    start_ms: e.start_ms,
                    end_ms: e.end_ms,
                }));
                fixed
            })
            .collect();
        let reason = reason(a, verdict, &basis);
        let unshifted = t
            .events
            .iter()
            .zip(each)
            .filter(|(_, s)| s.is_none())
            .map(|(e, _)| line_ref(e))
            .collect();
        out.push(Finding {
            id: 0,
            file: file.to_string(),
            style: String::new(),
            original: original(verdict),
            suggestion: suggestion(&stretches),
            reason,
            source: "timing".into(),
            occurrences,
            before: Vec::new(),
            after: Vec::new(),
            timing: Some(TimingFix {
                track: t.id,
                frame_ns: video.frame_ns,
                stretches: fixed,
                unshifted,
                basis,
            }),
        });
    }
    (out, notes)
}

/// What a finding rests on, in words: "cuts: 289 times, +1 in 7 of 9 windows", "shares 11268
/// times with track 5, all at the same frame".
fn reason(a: &Analysis, verdict: &Verdict, basis: &Basis) -> String {
    match basis {
        Basis::Cuts { .. } => match verdict {
            Verdict::Partial(s) => format!(
                "cuts: {} times, the offset changing {} times",
                a.times,
                s.len() - 1
            ),
            _ => format!(
                "cuts: {}",
                detail(a)
                    .unwrap_or_default()
                    .trim_start_matches('(')
                    .trim_end_matches(')')
            ),
        },
        Basis::Lines {
            source,
            shared,
            at_gap,
            gap,
        } => {
            let how_many = if at_gap == shared {
                "all".to_string()
            } else {
                format!("{at_gap} of them")
            };
            format!(
                "shares {shared} times with track {source}, {how_many} {}",
                apart(*gap)
            )
        }
    }
}

/// A stretch with its first and last lines and the frame that shows its error, when it moves any;
/// and the line that frame belongs to.
fn fix_stretch<'e>(
    video: &Video,
    events: &'e [Event],
    each: &[Option<usize>],
    k: usize,
    s: &Stretch,
) -> (FixStretch, Option<&'e Event>) {
    let mine: Vec<&Event> = events
        .iter()
        .zip(each)
        .filter(|(_, o)| **o == Some(k))
        .map(|(e, _)| e)
        .collect();
    let moves = s.frames != 0;
    let first = mine.iter().min_by_key(|e| (e.start_ms, e.end_ms));
    let last = mine.iter().max_by_key(|e| (e.end_ms, e.start_ms));
    let shown = moves.then(|| evidence(video, &mine, s)).flatten();
    let fixed = FixStretch {
        from_ms: s.from_ms,
        to_ms: s.to_ms,
        frames: s.frames,
        first: first.filter(|_| moves).map(|e| line_ref(e)),
        last: last.filter(|_| moves).map(|e| line_ref(e)),
        frame_ms: shown.map(|(_, time_ms)| shown_frame_ms(video, time_ms, s.frames)),
    };
    (fixed, shown.map(|(e, _)| e))
}

/// The line that shows a stretch's error best, and the time of it on a cut: of the lines with an
/// end at the stretch's offset (a late one still on the new shot's first frame), the one starting
/// nearest the stretch's middle; failing that, a start at the offset.
fn evidence<'e>(video: &Video, mine: &[&'e Event], s: &Stretch) -> Option<(&'e Event, i64)> {
    let from = s
        .from_ms
        .or_else(|| mine.iter().map(|e| e.start_ms).min())?;
    let to = s.to_ms.or_else(|| mine.iter().map(|e| e.end_ms).max())?;
    let middle = from + (to - from) / 2;
    let nearest = |times: &dyn Fn(&Event) -> i64| {
        mine.iter()
            .filter(|e| frame_offset(video, times(e)) == s.frames)
            .min_by_key(|e| ((e.start_ms - middle).abs(), e.start_ms))
            .map(|e| (*e, times(e)))
    };
    nearest(&|e| e.end_ms).or_else(|| nearest(&|e| e.start_ms))
}

/// The frame that shows the error of a time `frames` off the nearest cut, in ms: the cut's own
/// frame for a late time (the line still showing, or not yet), the frame before it for an early one.
fn shown_frame_ms(video: &Video, time_ms: i64, frames: i64) -> i64 {
    let cut = nearest_keyframe(video, time_ms);
    let ns = if frames > 0 {
        cut
    } else {
        cut - i128::from(video.frame_ns)
    };
    i64::try_from((ns + 500_000).div_euclid(1_000_000)).unwrap_or(i64::MAX)
}

fn line_ref(e: &Event) -> LineRef {
    LineRef {
        raw: e.raw.clone(),
        start_ms: e.start_ms,
        end_ms: e.end_ms,
    }
}

/// "1 frame late", or for a partial track "partial: 1 frame late, in sync, 1 frame late".
fn original(v: &Verdict) -> String {
    match v {
        Verdict::Partial(stretches) => {
            let labels: Vec<String> = stretches.iter().map(|s| shift_label(s.frames)).collect();
            format!("partial: {}", labels.join(", "))
        }
        v => verdict_label(v),
    }
}

/// "shift 1 frame earlier", or for a partial track each shift with its stretches, in order:
/// "shift 1 frame earlier: start – 0:07:55.78, 0:15:40.47 – end".
fn suggestion(stretches: &[Stretch]) -> String {
    if let [whole] = stretches {
        return format!("shift {}", move_label(whole.frames));
    }
    let mut shifts: Vec<(i64, Vec<String>)> = Vec::new();
    for s in stretches.iter().filter(|s| s.frames != 0) {
        let from = s.from_ms.map_or_else(|| "start".into(), text::timestamp);
        let to = s.to_ms.map_or_else(|| "end".into(), text::timestamp);
        let range = format!("{from} – {to}");
        match shifts.iter_mut().find(|(frames, _)| *frames == s.frames) {
            Some((_, ranges)) => ranges.push(range),
            None => shifts.push((s.frames, vec![range])),
        }
    }
    let parts: Vec<String> = shifts
        .iter()
        .map(|(frames, ranges)| format!("{}: {}", move_label(*frames), ranges.join(", ")))
        .collect();
    format!("shift {}", parts.join("; "))
}

/// What undoes `frames` of lateness: "1 frame earlier", "2 frames later".
fn move_label(frames: i64) -> String {
    frames_way(frames, "earlier", "later")
}

/// "at the same frame", "1 frame later", "2 frames earlier".
fn apart(gap: i64) -> String {
    if gap == 0 {
        return "at the same frame".into();
    }
    frames_way(gap, "later", "earlier")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timing::shared::{Follow, Shared};
    use crate::timing::{OFFSETS, analyse};

    /// 25 fps, a keyframe every 2 s.
    fn video() -> Video {
        Video {
            frame_ns: 40_000_000,
            keyframes_ns: (0..=900).map(|i| i * 2_000_000_000).collect(),
        }
    }

    /// `frames` frames after keyframe `n`, in ms.
    fn at(n: i64, frames: i64) -> i64 {
        n * 2000 + frames * 40
    }

    fn event(start_ms: i64, end_ms: i64) -> Event {
        Event {
            track: 5,
            start_ms,
            end_ms,
            layer: "0".into(),
            style: "D".into(),
            text: "Line.".into(),
            pos: None,
            karaoke: false,
            raw: format!("0,D,,0,0,0,,Line at {start_ms}."),
        }
    }

    #[test]
    fn a_late_track_gets_one_finding_opening_on_the_cut_its_middle_line_runs_past() {
        // A line every 6 s, each starting and ending a frame after a cut: 6 minutes, 2 windows.
        let events: Vec<Event> = (0..60)
            .map(|n| event(at(3 * n, 1), at(3 * n + 1, 1)))
            .collect();
        let a = analyse(&video(), &events);
        assert_eq!(a.verdict, Verdict::Shifted(1));
        let track = Track {
            id: 5,
            events: &events,
            analysis: &a,
            lines: None,
        };
        let (found, notes) = findings("/m/a.mkv", &video(), &[track]);
        assert!(notes.is_empty(), "{notes:?}");
        let [f] = found.as_slice() else {
            panic!("{found:?}")
        };
        assert_eq!(
            (
                f.original.as_str(),
                f.suggestion.as_str(),
                f.reason.as_str(),
                f.source.as_str()
            ),
            (
                "1 frame late",
                "shift 1 frame earlier",
                "cuts: 120 times, +1 in 2 of 2 windows",
                "timing"
            )
        );
        // The middle is 2:58.04; the line starting nearest it, at 3:00.04, ends at 3:02.04, a
        // frame after the cut at 3:02.
        assert_eq!(
            f.occurrences,
            [Occurrence {
                track: 5,
                start_ms: 180_040,
                end_ms: 182_040
            }]
        );
        let fix = f.timing.as_ref().unwrap();
        assert_eq!((fix.track, fix.frame_ns), (5, 40_000_000));
        assert_eq!(
            fix.stretches,
            [FixStretch {
                from_ms: None,
                to_ms: None,
                frames: 1,
                first: Some(line_ref(&events[0])),
                last: Some(line_ref(&events[59])),
                frame_ms: Some(182_000),
            }]
        );
        assert_eq!(
            fix.basis,
            Basis::Cuts {
                times: 120,
                agreeing: 2,
                windows: 2
            }
        );
    }

    fn analysis(verdict: Verdict, each: Vec<Option<usize>>) -> Analysis {
        Analysis {
            verdict,
            times: 200,
            pool: [0; OFFSETS],
            windows: 9,
            agreeing: 0,
            styles: Vec::new(),
            frame_ns: 40_000_000,
            each,
            unshifted: Vec::new(),
        }
    }

    fn stretch(from_ms: Option<i64>, to_ms: Option<i64>, frames: i64) -> Stretch {
        Stretch {
            from_ms,
            to_ms,
            frames,
        }
    }

    #[test]
    fn a_partial_track_records_each_stretch_and_the_lines_it_leaves_alone() {
        let events = [
            event(at(1, 5), at(2, 1)),
            event(at(3, 5), at(4, 1)),
            event(at(10, 5), at(11, 0)),
            // Left where it is.
            event(at(15, 5), at(16, 0)),
            event(at(20, 5), at(21, 1)),
        ];
        let a = analysis(
            Verdict::Partial(vec![
                stretch(None, Some(at(4, 1)), 1),
                stretch(Some(at(10, 5)), Some(at(11, 0)), 0),
                stretch(Some(at(20, 5)), None, 1),
            ]),
            vec![Some(0), Some(0), Some(1), None, Some(2)],
        );
        let track = Track {
            id: 5,
            events: &events,
            analysis: &a,
            lines: None,
        };
        let (found, _) = findings("/m/a.mkv", &video(), &[track]);
        let f = &found[0];
        assert_eq!(f.original, "partial: 1 frame late, in sync, 1 frame late");
        assert_eq!(
            f.suggestion,
            "shift 1 frame earlier: start – 0:00:08.04, 0:00:40.20 – end"
        );
        assert_eq!(f.reason, "cuts: 200 times, the offset changing 2 times");
        let fix = f.timing.as_ref().unwrap();
        assert_eq!(
            fix.stretches,
            [
                // Of the two lines, the second starts nearer the stretch's middle (0:00:05.12).
                FixStretch {
                    from_ms: None,
                    to_ms: Some(at(4, 1)),
                    frames: 1,
                    first: Some(line_ref(&events[0])),
                    last: Some(line_ref(&events[1])),
                    frame_ms: Some(8000),
                },
                FixStretch {
                    from_ms: Some(at(10, 5)),
                    to_ms: Some(at(11, 0)),
                    frames: 0,
                    first: None,
                    last: None,
                    frame_ms: None,
                },
                FixStretch {
                    from_ms: Some(at(20, 5)),
                    to_ms: None,
                    frames: 1,
                    first: Some(line_ref(&events[4])),
                    last: Some(line_ref(&events[4])),
                    frame_ms: Some(42_000),
                },
            ]
        );
        assert_eq!(fix.unshifted, [line_ref(&events[3])]);
        let starts: Vec<i64> = f.occurrences.iter().map(|o| o.start_ms).collect();
        assert_eq!(starts, [at(3, 5), at(20, 5)]);
    }

    #[test]
    fn an_early_stretch_opens_on_the_frame_before_the_cut() {
        // Ends a frame before a cut, at 0:00:23.96: the cut is 0:00:24, its frame before 23.96.
        let events = [event(at(10, 5), at(12, -1))];
        let a = analysis(Verdict::Shifted(-1), vec![Some(0)]);
        let track = Track {
            id: 5,
            events: &events,
            analysis: &a,
            lines: None,
        };
        let (found, _) = findings("/m/a.mkv", &video(), &[track]);
        assert_eq!(found[0].suggestion, "shift 1 frame later");
        let fix = found[0].timing.as_ref().unwrap();
        assert_eq!(fix.stretches[0].frame_ms, Some(23_960));
    }

    #[test]
    fn a_track_following_another_rests_on_their_shared_lines() {
        let events = [event(at(10, 25), at(12, 25))];
        let own = analysis(Verdict::TooFew, vec![None]);
        let follows = Lines::Follows(Follow {
            source: 2,
            gap: 0,
            shared: Shared {
                times: 34,
                counts: [0, 0, 0, 34, 0, 0, 0],
            },
            verdict: Verdict::Shifted(1),
            each: vec![Some(0)],
            unshifted: Vec::new(),
        });
        let track = Track {
            id: 4,
            events: &events,
            analysis: &own,
            lines: Some(&follows),
        };
        let (found, _) = findings("/m/a.mkv", &video(), &[track]);
        let f = &found[0];
        assert_eq!(
            (f.original.as_str(), f.reason.as_str()),
            (
                "1 frame late",
                "shares 34 times with track 2, all at the same frame"
            )
        );
        let fix = f.timing.as_ref().unwrap();
        assert_eq!(
            fix.basis,
            Basis::Lines {
                source: 2,
                shared: 34,
                at_gap: 34,
                gap: 0
            }
        );
        // No end or start of its own sits a frame off a cut: nothing to open on.
        assert_eq!(fix.stretches[0].frame_ms, None);
        assert!(f.occurrences.is_empty(), "{:?}", f.occurrences);
    }

    #[test]
    fn a_track_in_sync_or_contradicted_by_its_shared_lines_gets_no_finding() {
        let events = [event(at(10, 5), at(11, 1))];
        let in_sync = analysis(Verdict::InSync, vec![Some(0)]);
        let late = analysis(Verdict::Shifted(1), vec![Some(0)]);
        let disagrees = Lines::Disagrees {
            source: 5,
            says: Verdict::InSync,
        };
        let tracks = [
            Track {
                id: 3,
                events: &events,
                analysis: &in_sync,
                lines: None,
            },
            Track {
                id: 6,
                events: &events,
                analysis: &late,
                lines: Some(&disagrees),
            },
        ];
        let (found, notes) = findings("/m/a.mkv", &video(), &tracks);
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(
            notes,
            [
                "track 6: its times against the cuts say 1 frame late, its lines shared with track 5 say in sync: no fix"
            ]
        );
    }
}
