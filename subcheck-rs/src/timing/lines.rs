//! Which lines each stretch of a partial track moves. Lines move in blocks: events that overlap or
//! touch (one ends where the next starts, or layers with the same times), so that no join opens a
//! one-frame gap or overlap. The windows put each change of offset between two pooled times; a
//! block between them goes with the stretch its own times point to.

use super::{Stretch, Video, frame_offset};
use crate::text::Event;

/// Events that overlap or touch, earliest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub start_ms: i64,
    pub end_ms: i64,
    /// Indices into the track's events.
    pub events: Vec<usize>,
}

/// The track's events in blocks, in time order.
pub fn blocks(events: &[Event]) -> Vec<Block> {
    let times: Vec<(i64, i64)> = events.iter().map(|e| (e.start_ms, e.end_ms)).collect();
    blocks_of(&times)
}

/// Lines at these (start, end) times in blocks, in time order; a block's indices are into `times`.
pub fn blocks_of(times: &[(i64, i64)]) -> Vec<Block> {
    let mut order: Vec<usize> = (0..times.len()).collect();
    order.sort_by_key(|&i| times[i]);
    let mut out: Vec<Block> = Vec::new();
    for i in order {
        let (start_ms, end_ms) = times[i];
        match out.last_mut() {
            Some(b) if start_ms <= b.end_ms => {
                b.end_ms = b.end_ms.max(end_ms);
                b.events.push(i);
            }
            _ => out.push(Block {
                start_ms,
                end_ms,
                events: vec![i],
            }),
        }
    }
    out
}

/// Whether a start or end in the block sits at `offset` from a cut.
fn points_to(video: &Video, events: &[Event], block: &Block, offset: i64) -> bool {
    block.events.iter().any(|&i| {
        frame_offset(video, events[i].start_ms) == offset
            || frame_offset(video, events[i].end_ms) == offset
    })
}

/// What [`line_exact`] makes of a partial track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moves {
    /// The stretches with their ends on lines: the first line's start and the last line's end of
    /// what each one moves.
    pub stretches: Vec<Stretch>,
    /// Each event's stretch: `None` for an event left where it is, in a block whose times point
    /// both ways or against the side it falls on.
    pub each: Vec<Option<usize>>,
    /// The blocks left where they are, as (start, end) in ms.
    pub unshifted: Vec<(i64, i64)>,
}

/// Which stretch moves each event.
///
/// `stretches` are [`super::analyse`]'s, each change of offset between two pooled times. The
/// blocks between those times are split in two where the most of them agree with their side: a
/// block points to a stretch when one of its times sits at that stretch's offset. Of the splits
/// that agree as well, the one at the longest silence between blocks; a block with no evidence of
/// its own is nowhere near a cut, so a frame either way does not show.
pub fn line_exact(video: &Video, events: &[Event], stretches: &[Stretch]) -> Moves {
    let blocks = blocks(events);
    let gaps: Vec<(i64, i64)> = stretches
        .windows(2)
        .filter_map(|p| Some((p[0].to_ms?, p[1].from_ms?)))
        .collect();
    // Each block's stretch: how many gaps it is wholly past, until a gap's own blocks are split.
    let mut of: Vec<Option<usize>> = blocks
        .iter()
        .map(|b| Some(gaps.iter().filter(|&&(_, to)| b.start_ms > to).count()))
        .collect();
    for (g, &(from, to)) in gaps.iter().enumerate() {
        let here: Vec<usize> = (0..blocks.len())
            .filter(|&k| blocks[k].start_ms <= to && blocks[k].end_ms >= from)
            .collect();
        let Some(&first) = here.first() else {
            continue;
        };
        let (a, b) = (stretches[g].frames, stretches[g + 1].frames);
        let evidence: Vec<(bool, bool)> = here
            .iter()
            .map(|&k| {
                (
                    points_to(video, events, &blocks[k], a),
                    points_to(video, events, &blocks[k], b),
                )
            })
            .collect();
        // Splitting before here[s]: blocks pointing only to a before it, only to b from it on.
        let score = |s: usize| {
            evidence[..s]
                .iter()
                .filter(|&&e| e == (true, false))
                .count()
                + evidence[s..]
                    .iter()
                    .filter(|&&e| e == (false, true))
                    .count()
        };
        let silence = |s: usize| -> i64 {
            let k = first + s;
            match (k.checked_sub(1), blocks.get(k)) {
                (Some(before), Some(after)) => after.start_ms - blocks[before].end_ms,
                _ => i64::MAX,
            }
        };
        let best = (0..=here.len()).map(score).max().unwrap_or(0);
        let split = (0..=here.len())
            .filter(|&s| score(s) == best)
            .max_by_key(|&s| (silence(s), std::cmp::Reverse(s)))
            .unwrap_or(0);
        for (s, (&k, &(to_a, to_b))) in here.iter().zip(&evidence).enumerate() {
            of[k] = if s < split {
                (!to_b).then_some(g)
            } else {
                (!to_a).then_some(g + 1)
            };
        }
    }
    moves(&blocks, &of, events.len(), stretches)
}

/// [`Moves`] from each block's stretch: the stretches' ends on the blocks they move.
pub(super) fn moves(
    blocks: &[Block],
    of: &[Option<usize>],
    events: usize,
    stretches: &[Stretch],
) -> Moves {
    let mut moved: Vec<Stretch> = stretches.to_vec();
    for (i, s) in moved.iter_mut().enumerate() {
        let mine = blocks.iter().zip(of).filter(|(_, o)| **o == Some(i));
        if i > 0 {
            s.from_ms = mine.clone().map(|(b, _)| b.start_ms).min().or(s.from_ms);
        }
        if i + 1 < stretches.len() {
            s.to_ms = mine.map(|(b, _)| b.end_ms).max().or(s.to_ms);
        }
    }
    let mut each = vec![None; events];
    let mut unshifted = Vec::new();
    for (b, o) in blocks.iter().zip(of) {
        for &i in &b.events {
            each[i] = *o;
        }
        if o.is_none() {
            unshifted.push((b.start_ms, b.end_ms));
        }
    }
    Moves {
        stretches: moved,
        each,
        unshifted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(start_ms: i64, end_ms: i64) -> Event {
        Event {
            track: 0,
            start_ms,
            end_ms,
            layer: "0".into(),
            style: "D".into(),
            text: "Line.".into(),
            pos: None,
            karaoke: false,
            raw: "0,D,,0,0,0,,Line.".into(),
        }
    }

    #[test]
    fn events_that_overlap_or_touch_are_one_block() {
        let events = [
            event(5000, 6000),
            event(1000, 2000),
            // Touches the line before: one block.
            event(2000, 3000),
            // Another layer of the same line.
            event(1000, 2000),
            // Overlaps the first block's last line.
            event(2500, 3500),
            event(3600, 4000),
        ];
        let spans: Vec<(i64, i64, Vec<usize>)> = blocks(&events)
            .into_iter()
            .map(|b| (b.start_ms, b.end_ms, b.events))
            .collect();
        assert_eq!(
            spans,
            [
                (1000, 3500, vec![1, 3, 2, 4]),
                (3600, 4000, vec![5]),
                (5000, 6000, vec![0])
            ]
        );
    }

    /// K-On!! E16's video keyframes (ms) around its two changes of offset, from mkvextract's
    /// cues.
    const E16_KEYFRAMES_MS: &[u64] = &[
        450_825, 451_326, 451_868, 452_953, 453_954, 454_579, 455_246, 456_331, 457_332, 458_333,
        459_334, 460_335, 461_336, 462_337, 463_088, 463_838, 464_923, 465_924, 466_925, 467_676,
        468_343, 469_386, 470_053, 470_595, 471_638, 472_639, 473_682, 474_683, 475_684, 476_518,
        477_310, 478_395, 479_187, 479_938, 481_022, 482_023, 482_691, 483_400, 484_442, 485_443,
        486_236, 486_987, 487_988, 488_863, 489_906, 490_907, 491_950, 492_867, 493_702, 494_786,
        495_662, 496_454, 497_497, 498_498, 499_332, 930_596, 931_431, 932_098, 932_557, 933_266,
        933_808, 934_851, 935_852, 936_853, 937_437, 937_937, 938_521, 939_021, 940_064, 941_065,
        941_983, 942_900, 943_901, 944_777, 945_445, 946_028, 947_113, 948_114, 949_115, 949_866,
        950_616, 951_701, 952_702, 953_703, 954_704, 955_705,
    ];

    /// E16's English Full track around the changes: every line's start and end (ms).
    const E16_LINES: &[(i64, i64)] = &[
        (452_520, 454_060),
        (454_580, 455_270),
        (455_540, 457_060),
        (457_060, 458_880),
        (460_440, 462_850),
        (464_500, 466_260),
        (467_070, 468_360),
        // A thought, its times nowhere near a cut.
        (471_960, 475_780),
        // Four lines end to end, the last ending on a cut.
        (483_510, 484_960),
        (484_960, 486_040),
        (486_040, 487_460),
        (487_460, 488_840),
        (490_280, 493_680),
        (493_960, 496_430),
        (496_700, 498_320),
        (930_720, 932_530),
        (933_020, 935_420),
        (935_420, 937_340),
        (937_340, 937_910),
        (938_150, 939_000),
        // Ends a frame after a cut: the late stretch starts here.
        (940_470, 942_920),
        (943_070, 944_920),
        (946_370, 947_500),
        (947_500, 949_130),
        (949_130, 950_520),
        (951_440, 953_320),
        (953_500, 954_310),
        (954_310, 955_420),
    ];

    fn e16() -> (Video, Vec<Event>) {
        let video = Video {
            frame_ns: 41_708_333,
            keyframes_ns: E16_KEYFRAMES_MS.iter().map(|k| k * 1_000_000).collect(),
        };
        let events = E16_LINES.iter().map(|&(s, e)| event(s, e)).collect();
        (video, events)
    }

    fn stretch(from_ms: Option<i64>, to_ms: Option<i64>, frames: i64) -> Stretch {
        Stretch {
            from_ms,
            to_ms,
            frames,
        }
    }

    #[test]
    fn e16s_stretches_end_on_lines_and_its_gap_lines_go_where_they_point() {
        let (video, events) = e16();
        // What the windows give: the pooled times either side of each change.
        let pooled = [
            stretch(None, Some(468_360), 1),
            stretch(Some(488_840), Some(939_000), 0),
            stretch(Some(942_920), None, 1),
        ];
        let Moves {
            stretches, each, ..
        } = line_exact(&video, &events, &pooled);
        assert_eq!(
            stretches,
            [
                // The thought is 3.6 s from the late line before it and 7.7 s from the in-sync
                // run after it: the longer silence is after it.
                stretch(None, Some(475_780), 1),
                stretch(Some(483_510), Some(939_000), 0),
                stretch(Some(940_470), None, 1),
            ]
        );
        let at = |start| each[E16_LINES.iter().position(|l| l.0 == start).unwrap()];
        assert_eq!(at(471_960), Some(0));
        assert_eq!(at(483_510), Some(1));
        assert_eq!(at(487_460), Some(1));
        assert_eq!(at(938_150), Some(1));
        assert_eq!(at(940_470), Some(2));
        assert!(each.iter().all(Option::is_some));
    }

    /// 25 fps, a keyframe every 2 s.
    fn video_2s() -> Video {
        Video {
            frame_ns: 40_000_000,
            keyframes_ns: (0..=100).map(|i| i * 2_000_000_000).collect(),
        }
    }

    /// `frames` frames after keyframe `n` of [`video_2s`], in ms.
    fn at(n: i64, frames: i64) -> i64 {
        n * 2000 + frames * 40
    }

    #[test]
    fn a_block_pointing_both_ways_stays_where_it_is() {
        let events = [
            // Late: ends a frame after a cut.
            event(at(10, 12), at(11, 1)),
            // Starts a frame after one cut and ends on another.
            event(at(12, 1), at(13, 0)),
            // In sync: ends on a cut.
            event(at(14, 12), at(15, 0)),
        ];
        let pooled = [
            stretch(None, Some(at(11, 1)), 1),
            stretch(Some(at(15, 0)), None, 0),
        ];
        let m = line_exact(&video_2s(), &events, &pooled);
        assert_eq!(m.each, [Some(0), None, Some(1)]);
        assert_eq!(m.unshifted, [(at(12, 1), at(13, 0))]);
        assert_eq!(
            m.stretches,
            [
                stretch(None, Some(at(11, 1)), 1),
                stretch(Some(at(14, 12)), None, 0),
            ]
        );
    }

    #[test]
    fn a_block_against_its_side_stays_where_it_is() {
        // Between the two pooled times: one line pointing to the in-sync stretch, then one
        // pointing to the late one. One of them is against any split.
        let events = [
            event(at(10, 12), at(11, 1)),
            event(at(12, 12), at(13, 0)),
            event(at(14, 12), at(15, 1)),
            event(at(16, 12), at(17, 0)),
        ];
        let pooled = [
            stretch(None, Some(at(11, 1)), 1),
            stretch(Some(at(17, 0)), None, 0),
        ];
        let each = line_exact(&video_2s(), &events, &pooled).each;
        // Splits after the first or the third line agree with three of four blocks; the silences
        // are equal, so the earlier split wins.
        assert_eq!(each, [Some(0), Some(1), None, Some(1)]);
    }
}
