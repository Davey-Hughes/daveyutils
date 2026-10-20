//! Lines two tracks of one file share, and how many frames apart they are. A Signs/Songs track
//! repeats lines of its Full track (signs, songs, the OP and ED) but rarely has enough of them on
//! cuts for a verdict of its own (K-On!!: 21 of 24 episodes). Where its lines are another track's
//! to the frame, it is timed as that track is.

use std::collections::{HashMap, HashSet};

use super::lines;
use super::{Analysis, OFFSETS, Stretch, Verdict, Video, decided, frame_index, slot, within};
use crate::text::Event;

/// How one track's lines sit against the same lines in another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shared {
    /// Distinct times (per style, start or end) of lines with a twin, at most 3 frames apart.
    pub times: usize,
    /// How many of them sit at each gap, this track's frame less the other's, -3 to +3.
    pub counts: [usize; OFFSETS],
}

impl Shared {
    /// The gap the shared times agree on: at least 10 of them and twice the runner-up, as for an
    /// offset.
    pub fn gap(&self) -> Option<i64> {
        decided(&self.counts)
    }

    /// How many shared times sit at `gap`.
    pub fn at(&self, gap: i64) -> usize {
        self.counts[slot(gap)]
    }
}

/// Each event's twin in `other`: the one event there with the same raw text starting within 3
/// frames of it. `None` when there is none, or more than one: a sign repeated frame by frame would
/// match its neighbour as well as itself, and a shift would read as no shift.
pub fn twins(video: &Video, track: &[Event], other: &[Event]) -> Vec<Option<usize>> {
    let mut by_raw: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, e) in other.iter().enumerate() {
        by_raw.entry(e.raw.as_str()).or_default().push(i);
    }
    track
        .iter()
        .map(|e| {
            let at = frame_index(video, e.start_ms);
            let mut near = by_raw
                .get(e.raw.as_str())?
                .iter()
                .filter(|&&i| within(frame_index(video, other[i].start_ms) - at).is_some());
            let one = *near.next()?;
            near.next().is_none().then_some(one)
        })
        .collect()
}

/// The gaps between `track`'s lines and their twins in `other`.
pub fn shared(video: &Video, track: &[Event], other: &[Event], twins: &[Option<usize>]) -> Shared {
    let mut seen = HashSet::new();
    let mut counts = [0; OFFSETS];
    for (e, twin) in track.iter().zip(twins) {
        let Some(t) = twin.map(|i| &other[i]) else {
            continue;
        };
        for (is_end, mine, theirs) in [(false, e.start_ms, t.start_ms), (true, e.end_ms, t.end_ms)]
        {
            if !seen.insert((e.style.as_str(), is_end, mine)) {
                continue;
            }
            if let Some(gap) = within(frame_index(video, mine) - frame_index(video, theirs)) {
                counts[slot(gap)] += 1;
            }
        }
    }
    Shared {
        times: counts.iter().sum(),
        counts,
    }
}

/// A track timed as another is, `gap` frames after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Follow {
    /// The other track's mkvmerge ID.
    pub source: usize,
    pub gap: i64,
    pub shared: Shared,
    /// The other track's verdict, `gap` frames later.
    pub verdict: Verdict,
    /// As [`Analysis::each`], for this track's events.
    pub each: Vec<Option<usize>>,
    /// As [`Analysis::unshifted`].
    pub unshifted: Vec<(i64, i64)>,
}

/// What a track's shared lines say about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lines {
    /// It has no verdict of its own and takes another track's.
    Follows(Follow),
    /// Its own whole-track verdict is not what its lines shared with `source` say.
    Disagrees { source: usize, says: Verdict },
}

/// What the file's tracks' shared lines say, track by track. Each track is compared with the
/// other tracks that have a verdict, and the one it shares the most times with at an agreed gap
/// is used (on a tie, the smaller gap, then the lower ID). A track with too few lines or an
/// unclear verdict follows it. A shifted or in-sync track is checked against it when that track is
/// shifted or in sync too; a partial one is not checked.
///
/// `tracks`: each track's mkvmerge ID, events and analysis.
pub fn lines_of(video: &Video, tracks: &[(usize, &[Event], &Analysis)]) -> Vec<Option<Lines>> {
    tracks
        .iter()
        .map(|&(id, mine, own)| {
            // A partial track's verdict is not checked against shared lines: nothing to compute.
            if matches!(own.verdict, Verdict::Partial(_)) {
                return None;
            }
            let (source, theirs, twins, shared, gap) = tracks
                .iter()
                .filter(|&&(other, _, a)| other != id && is_decided(&a.verdict))
                .filter_map(|&(other, events, a)| {
                    let twins = twins(video, mine, events);
                    let shared = shared(video, mine, events, &twins);
                    let gap = shared.gap()?;
                    Some((other, a, twins, shared, gap))
                })
                .max_by_key(|(other, _, _, s, gap)| {
                    (
                        s.times,
                        std::cmp::Reverse(gap.abs()),
                        std::cmp::Reverse(*other),
                    )
                })?;
            match (&own.verdict, offset(&theirs.verdict)) {
                (Verdict::TooFew | Verdict::Unclear, _) => {
                    follow(source, mine, theirs, &twins, gap, shared).map(Lines::Follows)
                }
                (mine @ (Verdict::InSync | Verdict::Shifted(_)), Some(theirs)) => {
                    let says = super::whole(theirs + gap);
                    (says != *mine).then_some(Lines::Disagrees { source, says })
                }
                _ => None,
            }
        })
        .collect()
}

fn is_decided(v: &Verdict) -> bool {
    matches!(
        v,
        Verdict::InSync | Verdict::Shifted(_) | Verdict::Partial(_)
    )
}

/// A whole-track verdict's offset.
fn offset(v: &Verdict) -> Option<i64> {
    match v {
        Verdict::InSync => Some(0),
        Verdict::Shifted(frames) => Some(*frames),
        _ => None,
    }
}

/// `source`'s verdict for a track whose events are `mine`, `gap` frames after it. Off a partial
/// source, each of the track's blocks goes with its twins' stretch (twins that disagree leave it
/// where it is), and a block without twins with the stretch it starts in.
fn follow(
    source: usize,
    mine: &[Event],
    theirs: &Analysis,
    twins: &[Option<usize>],
    gap: i64,
    shared: Shared,
) -> Option<Follow> {
    let (verdict, each, unshifted) = match &theirs.verdict {
        Verdict::InSync | Verdict::Shifted(_) => {
            let frames = offset(&theirs.verdict)? + gap;
            (super::whole(frames), vec![Some(0); mine.len()], Vec::new())
        }
        Verdict::Partial(stretches) => {
            let blocks = lines::blocks(mine);
            let of: Vec<Option<usize>> = blocks
                .iter()
                .map(|b| {
                    let mut theirs_of = b
                        .events
                        .iter()
                        .filter_map(|&i| twins[i])
                        .map(|t| theirs.each[t]);
                    match theirs_of.next() {
                        Some(first) => theirs_of.all(|s| s == first).then_some(first).flatten(),
                        None => stretches
                            .iter()
                            .rposition(|s| s.from_ms.is_none_or(|from| from <= b.start_ms)),
                    }
                })
                .collect();
            let moved: Vec<Stretch> = stretches
                .iter()
                .map(|s| Stretch {
                    frames: s.frames + gap,
                    ..*s
                })
                .collect();
            let m = lines::moves(&blocks, &of, mine.len(), &moved);
            // None of this track's lines in a stretch that moves: the track is in sync.
            let shifts = |k: usize| m.stretches[k].frames != 0;
            if m.each.iter().flatten().any(|&k| shifts(k)) {
                (Verdict::Partial(m.stretches), m.each, m.unshifted)
            } else {
                (Verdict::InSync, vec![Some(0); mine.len()], Vec::new())
            }
        }
        Verdict::TooFew | Verdict::Unclear => return None,
    };
    Some(Follow {
        source,
        gap,
        shared,
        verdict,
        each,
        unshifted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 25 fps, a keyframe every 2 s.
    fn video() -> Video {
        Video {
            frame_ns: 40_000_000,
            keyframes_ns: (0..=900).map(|i| i * 2_000_000_000).collect(),
        }
    }

    fn event(text: &str, start_ms: i64, end_ms: i64) -> Event {
        Event {
            track: 0,
            start_ms,
            end_ms,
            layer: "0".into(),
            style: "Sign".into(),
            text: text.into(),
            pos: None,
            karaoke: false,
            raw: format!("0,Sign,,0,0,0,,{text}"),
        }
    }

    /// `n` signs, one every 10 s, each 3 s long, `frames` frames after the same signs of `lines(n, 0)`.
    fn signs(n: i64, frames: i64) -> Vec<Event> {
        (0..n)
            .map(|i| {
                let start = i * 10_000 + 500 + frames * 40;
                event(&format!("Sign {i}"), start, start + 3000)
            })
            .collect()
    }

    #[test]
    fn a_twin_is_the_one_line_with_the_same_raw_text_within_three_frames() {
        let other = [
            event("A", 1000, 2000),
            event("B", 5000, 6000),
            // The same sign on two frames in a row: neither is anyone's only twin.
            event("C", 9000, 9040),
            event("C", 9040, 9080),
        ];
        let track = [
            event("A", 1040, 2040),
            // Four frames from its namesake: too far.
            event("B", 5160, 6160),
            event("C", 9040, 9080),
            event("D", 1000, 2000),
        ];
        assert_eq!(twins(&video(), &track, &other), [Some(0), None, None, None]);
    }

    #[test]
    fn shared_lines_agree_on_a_gap_with_ten_times_and_twice_the_runner_up() {
        let v = video();
        let theirs = signs(5, 0);
        // Five signs a frame later: 10 times at +1.
        let later = signs(5, 1);
        let s = shared(&v, &later, &theirs, &twins(&v, &later, &theirs));
        assert_eq!((s.times, s.at(1), s.gap()), (10, 10, Some(1)));
        // Four of them: 8 times, too few.
        let fewer = signs(4, 1);
        let s = shared(&v, &fewer, &theirs, &twins(&v, &fewer, &theirs));
        assert_eq!((s.times, s.gap()), (8, None));
        // The same signs: gap 0.
        let same = signs(5, 0);
        let s = shared(&v, &same, &theirs, &twins(&v, &same, &theirs));
        assert_eq!(s.gap(), Some(0));
    }

    fn analysis(verdict: Verdict, each: Vec<Option<usize>>) -> Analysis {
        Analysis {
            verdict,
            times: 0,
            pool: [0; OFFSETS],
            windows: 0,
            agreeing: 0,
            styles: Vec::new(),
            frame_ns: 40_000_000,
            each,
            unshifted: Vec::new(),
        }
    }

    #[test]
    fn an_undecided_track_follows_the_track_it_shares_most_with() {
        let v = video();
        let full = signs(20, 0);
        let few = signs(6, 0);
        let thin = signs(20, 0);
        let late = analysis(Verdict::Shifted(1), vec![Some(0); 20]);
        let in_sync = analysis(Verdict::InSync, vec![Some(0); 6]);
        let unclear = analysis(Verdict::Unclear, vec![None; 20]);
        let out = lines_of(
            &v,
            &[(2, &full, &late), (3, &few, &in_sync), (5, &thin, &unclear)],
        );
        // Track 5 shares 40 times with track 2 and 12 with track 3 (both at gap 0): it follows 2.
        let Some(Lines::Follows(f)) = &out[2] else {
            panic!("{out:?}")
        };
        assert_eq!((f.source, f.gap, f.shared.times), (2, 0, 40));
        assert_eq!(f.verdict, Verdict::Shifted(1));
        assert_eq!(f.each, vec![Some(0); 20]);
        // Track 3's own verdict, in sync, is not what its lines shared with track 2 say.
        assert_eq!(
            out[1],
            Some(Lines::Disagrees {
                source: 2,
                says: Verdict::Shifted(1)
            })
        );
        // And track 2's, a frame late, is not what its lines shared with track 3 say: track 5 has
        // no verdict to compare with. Neither track's own verdict stands.
        assert_eq!(
            out[0],
            Some(Lines::Disagrees {
                source: 3,
                says: Verdict::InSync
            })
        );
    }

    #[test]
    fn a_track_a_frame_apart_from_its_source_gets_a_shift_of_its_own() {
        let v = video();
        let full = signs(20, 0);
        let signs_late = signs(20, 1);
        let late = analysis(Verdict::Shifted(1), vec![Some(0); 20]);
        let unclear = analysis(Verdict::TooFew, vec![None; 20]);
        let out = lines_of(&v, &[(5, &full, &late), (6, &signs_late, &unclear)]);
        let Some(Lines::Follows(f)) = &out[1] else {
            panic!("{out:?}")
        };
        assert_eq!((f.gap, f.verdict.clone()), (1, Verdict::Shifted(2)));
    }

    #[test]
    fn off_a_partial_source_a_track_with_no_line_in_a_stretch_that_moves_is_in_sync() {
        let v = video();
        // The source: late to 0:40, in sync after. This track's signs are all after 0:40.
        let full = signs(20, 0);
        let mut each: Vec<Option<usize>> = vec![Some(0); 4];
        each.extend(vec![Some(1); 16]);
        let partial = analysis(
            Verdict::Partial(vec![
                Stretch {
                    from_ms: None,
                    to_ms: Some(33_500),
                    frames: 1,
                },
                Stretch {
                    from_ms: Some(40_500),
                    to_ms: None,
                    frames: 0,
                },
            ]),
            each,
        );
        let thin: Vec<Event> = full[4..].to_vec();
        let unclear = analysis(Verdict::Unclear, vec![None; 16]);
        let out = lines_of(&v, &[(5, &full, &partial), (6, &thin, &unclear)]);
        let Some(Lines::Follows(f)) = &out[1] else {
            panic!("{out:?}")
        };
        assert_eq!(f.verdict, Verdict::InSync);
    }

    #[test]
    fn off_a_partial_source_each_block_goes_with_its_twins_stretch() {
        let v = video();
        // Ten signs late, then ten in sync: the source's stretches end on lines.
        let full = signs(20, 0);
        let mut each: Vec<Option<usize>> = vec![Some(0); 10];
        each.extend(vec![Some(1); 10]);
        // One sign the source left where it was.
        each[15] = None;
        let partial = analysis(
            Verdict::Partial(vec![
                Stretch {
                    from_ms: None,
                    to_ms: Some(93_500),
                    frames: 1,
                },
                Stretch {
                    from_ms: Some(100_500),
                    to_ms: None,
                    frames: 0,
                },
            ]),
            each,
        );
        // This track has the signs and one line of its own at 0:55, between the 6th and 7th.
        let mut thin = full.clone();
        thin.push(event("Own", 55_000, 56_000));
        let unclear = analysis(Verdict::Unclear, vec![None; 21]);
        let out = lines_of(&v, &[(5, &full, &partial), (6, &thin, &unclear)]);
        let Some(Lines::Follows(f)) = &out[1] else {
            panic!("{out:?}")
        };
        assert_eq!(
            f.verdict,
            Verdict::Partial(vec![
                Stretch {
                    from_ms: None,
                    to_ms: Some(93_500),
                    frames: 1,
                },
                Stretch {
                    from_ms: Some(100_500),
                    to_ms: None,
                    frames: 0,
                },
            ])
        );
        let mut want: Vec<Option<usize>> = vec![Some(0); 10];
        want.extend(vec![Some(1); 10]);
        want[15] = None;
        // The line of its own starts in the first stretch.
        want.push(Some(0));
        assert_eq!(f.each, want);
        assert_eq!(f.unshifted, [(150_500, 153_500)]);
    }

    #[test]
    fn frames_at_either_end_of_time_are_far_apart_not_an_overflow() {
        // At 1 ns a frame, a line at the end of i64 ms is more frames in than an i64 counts.
        let video = Video {
            frame_ns: 1,
            keyframes_ns: (0..=10).map(|i| i * 1_000_000_000).collect(),
        };
        let mine = [event("Sign", -1000, -500)];
        let theirs = [event("Sign", i64::MAX, i64::MAX)];
        assert_eq!(twins(&video, &mine, &theirs), [None]);
        assert_eq!(shared(&video, &mine, &theirs, &[Some(0)]).times, 0);
        // Two such lines are as far apart as they are, not both at the last frame an i64 counts.
        let late = [event("Sign", i64::MAX - 1, i64::MAX)];
        assert_eq!(twins(&video, &theirs, &late), [None]);
    }
}
