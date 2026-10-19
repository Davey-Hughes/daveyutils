//! A timing fix on its track: every event of a stretch that moves, moved by whole frames on the
//! video's frame grid, each time to the same place in its new frame. Pure: the fix, the blocks and
//! the grid in, new times or an outcome out.

use std::collections::HashSet;

use super::patch::{Outcome, Track};
use crate::store::{Finding, FixStretch, LineRef, Run, State, TimingFix};
use crate::subs::{self, Kind};
use crate::text;
use crate::timing::Video;

/// The run's accepted timing findings, in run order.
pub fn fixes(run: &Run) -> Vec<&Finding> {
    run.findings
        .iter()
        .filter(|f| f.timing.is_some() && run.state_of(f.id) == State::Accepted)
        .collect()
}

/// The video's frames: frame n (counted from the first keyframe) at `phase + n * frame`, stored
/// rounded to the timestamp scale, as mkvmerge stores them. `frame` is the exact duration the
/// DefaultDuration stands for, and `phase` the one that puts every keyframe on a frame: K-On!!
/// E09's frames are at 1001/24 ms each less 0.06 ms, so a frame whose exact time ends in .5 to .55
/// ms is stored a millisecond lower than its plain rounding, and a time a millisecond past it reads
/// as the frame before. In ticks of `1/den` ns.
#[derive(Debug, Clone, Copy)]
pub struct Grid {
    phase: i128,
    frame: i128,
    den: i128,
    /// TimestampScale, in ns.
    scale: i128,
    /// The phases that put every keyframe on a frame, `[lo, hi)`; `phase` is their middle.
    lo: i128,
    hi: i128,
}

/// Frame rates whose frame duration is no whole number of nanoseconds, as (frames, seconds): a
/// DefaultDuration within a nanosecond of one stands for it.
const RATES: [(i128, i128); 13] = [
    (24_000, 1001),
    (30_000, 1001),
    (48_000, 1001),
    (60_000, 1001),
    (120_000, 1001),
    (24, 1),
    (25, 1),
    (30, 1),
    (48, 1),
    (50, 1),
    (60, 1),
    (100, 1),
    (120, 1),
];

impl Grid {
    /// The grid of `video`'s frames, timestamps at `scale`; an error when no phase puts every
    /// keyframe on a frame (the frames are on no one grid, or not at the DefaultDuration).
    pub fn fit(video: &Video, scale: u64) -> Result<Grid, String> {
        let frame_ns = i128::from(video.frame_ns.max(1));
        let (frame, den) = RATES
            .iter()
            .map(|&(frames, seconds)| (1_000_000_000 * seconds, frames))
            .find(|&(ns, den)| (ns - frame_ns * den).abs() < den)
            .unwrap_or((frame_ns, 1));
        let scale = i128::from(scale.max(1));
        let first = video.keyframes_ns.first().map_or(0, |&k| i128::from(k));
        // Each keyframe stored at `k` was at a time in [k - scale/2, k + scale/2).
        let (mut lo, mut hi) = (i128::MIN, i128::MAX);
        for &k in &video.keyframes_ns {
            let k = i128::from(k);
            let n = ((k - first) * den * 2 + frame).div_euclid(frame * 2);
            lo = lo.max((k * 2 - scale) * den / 2 - n * frame);
            hi = hi.min((k * 2 + scale) * den / 2 - n * frame);
        }
        if lo >= hi {
            return Err("the video's keyframes are on no one frame grid".into());
        }
        Ok(Grid {
            phase: lo + (hi - lo) / 2,
            frame,
            den,
            scale,
            lo,
            hi,
        })
    }

    /// Frame `n`'s time, in ns.
    fn at(&self, n: i64) -> i128 {
        let t = self.phase + i128::from(n) * self.frame;
        let unit = self.den * self.scale;
        (t * 2 + unit).div_euclid(unit * 2) * self.scale
    }

    /// The frame a line starting or ending at `x` starts or stops showing on: the first at or after
    /// it, `T(n-1) < x ≤ T(n)`.
    pub fn frame_of(&self, x: i128) -> i64 {
        let mut n = i64::try_from((x * self.den - self.phase).div_euclid(self.frame)).unwrap_or(0);
        while self.at(n) < x {
            n += 1;
        }
        while self.at(n - 1) >= x {
            n -= 1;
        }
        n
    }

    /// `x` moved `frames` frames earlier (later when negative), to the same place in its new
    /// frame, rounded to `precision` and kept inside that frame: rounding never makes it another.
    /// Never before 0: a line at the very start stays there.
    pub fn moved(&self, x: i128, frames: i64, precision: i128) -> i128 {
        let n = self.frame_of(x);
        let m = n - frames;
        let y = x - (self.at(n) - self.at(m));
        let rounded = (y + precision / 2).div_euclid(precision) * precision;
        // The multiples of `precision` in (T(m-1), T(m)]: none when a frame is shorter than that.
        let (lo, hi) = (self.at(m - 1), self.at(m));
        let first = lo.div_euclid(precision) * precision + precision;
        let last = hi.div_euclid(precision) * precision;
        let kept = if first <= last {
            rounded.clamp(first, last)
        } else {
            rounded
        };
        kept.max(0)
    }

    /// [`Grid::moved`], when every phase the keyframes allow moves `x` to the same time; `None`
    /// when they leave it a millisecond apart. Keyframes that all sit at one place in a
    /// millisecond (a fixed GOP's at 23.976 fps) pin the phase only to within one.
    pub fn moved_surely(&self, x: i128, frames: i64, precision: i128) -> Option<i128> {
        let moved = self.moved(x, frames, precision);
        let n = self.frame_of(x);
        let m = n - frames;
        // The move reads frames n-1, n, m-1 and m, each of which another phase may make a frame
        // either way: one phase from each stretch where none of their stored times changes.
        let unit = self.den * self.scale;
        let mut phases = vec![self.lo];
        for k in (n - 2..=n + 1).chain(m - 2..=m + 1) {
            // Frame k's stored time steps up where phase + k * frame + unit / 2 is a multiple of
            // `unit`: the first such phase above `lo`.
            let base = i128::from(k) * self.frame + unit / 2;
            let step = self.lo + 1 + (-(self.lo + 1 + base)).rem_euclid(unit);
            if step < self.hi {
                phases.push(step);
            }
        }
        phases
            .into_iter()
            .all(|phase| Grid { phase, ..*self }.moved(x, frames, precision) == moved)
            .then_some(moved)
    }
}

/// How finely a track's times are written: ASS and SSA in centiseconds, SRT in milliseconds. In
/// nanoseconds.
pub fn precision(kind: Kind) -> i128 {
    match kind {
        Kind::Ass | Kind::Ssa => 10_000_000,
        Kind::Srt => 1_000_000,
    }
}

/// Block `i` of `track` as a line is recorded (`text::event`): its raw text (all but ReadOrder)
/// and times in ms. These are the times moved, too: the ones the scan recorded and the writer
/// writes.
fn line(track: &Track, i: usize) -> (String, i64, i64) {
    let b = &track.blocks[i];
    let payload = subs::c_string(&b.payload);
    let (start, end) = text::event_times(b, track.scale, track.default_duration);
    (text::raw(track.kind, &payload).to_string(), start, end)
}

/// A recorded line (its raw form, ReadOrder left out) as its fields before the text and its text:
/// for ASS and SSA, Layer to Effect, with their commas; for SRT nothing, and the whole line.
pub fn fields_and_text(kind: Kind, raw: &str) -> (&str, &str) {
    match kind {
        Kind::Ass | Kind::Ssa => raw
            .match_indices(',')
            .nth(6)
            .map_or(("", raw), |(i, _)| raw.split_at(i + 1)),
        Kind::Srt => ("", raw),
    }
}

/// A line as it is known on its track: by the fields before its text and its times.
fn key(kind: Kind, raw: &str, start_ms: i64, end_ms: i64) -> (&str, i64, i64) {
    (fields_and_text(kind, raw).0, start_ms, end_ms)
}

fn ms(ns: i128) -> i64 {
    i64::try_from(ns.div_euclid(1_000_000)).unwrap_or(i64::MAX)
}

/// The stretch an event starting at `start_ms` belongs to: the last that starts at or before it.
/// A block between two stretches (one the scan did not see, a drawing say) goes with the first.
fn stretch_of(stretches: &[FixStretch], start_ms: i64) -> Option<&FixStretch> {
    stretches
        .iter()
        .rev()
        .find(|s| s.from_ms.is_none_or(|from| from <= start_ms))
}

/// The stretch that moves a line of these times, if one does: not an unshifted line's. The scan
/// leaves lines where they are a block of overlapping lines at a time, so an event at an unshifted
/// line's times, whatever its text (a typo fixed since, a drawing), is in that block.
fn moving(fix: &TimingFix, start_ms: i64, end_ms: i64) -> Option<&FixStretch> {
    let unshifted = fix
        .unshifted
        .iter()
        .any(|l| l.start_ms == start_ms && l.end_ms == end_ms);
    stretch_of(&fix.stretches, start_ms).filter(|s| s.frames != 0 && !unshifted)
}

/// Whether `fix` moves a line of these times, frame grid or not (a line shown on no frame, which
/// stays where it is, counts as moved here).
pub fn moves(fix: &TimingFix, start_ms: i64, end_ms: i64) -> bool {
    moving(fix, start_ms, end_ms).is_some()
}

/// A track's times as a timing fix moves them: for looking up a typo found before the shift in
/// a track remuxed with it.
#[derive(Debug, Clone, Copy)]
pub struct Shifts<'a> {
    pub fix: &'a TimingFix,
    pub grid: Grid,
    pub precision: i128,
}

impl<'a> Shifts<'a> {
    /// `fix` on a track of `kind`, on the file's frame grid.
    pub fn new(fix: &'a TimingFix, grid: Grid, kind: Kind) -> Shifts<'a> {
        Shifts {
            fix,
            grid,
            precision: precision(kind),
        }
    }

    /// The stretch that moves a line of these times, if one does. A line with time to show but
    /// shown on no frame (both its times before one frame) stays where it is: moved, it could come
    /// to no time, and an SRT line of none is dropped by mkvmerge. One of no time moves (an SRT
    /// line with no duration is shown until the next).
    fn stretch(&self, start_ms: i64, end_ms: i64) -> Option<&FixStretch> {
        let frame = |t: i64| self.grid.frame_of(i128::from(t) * 1_000_000);
        moving(self.fix, start_ms, end_ms)
            .filter(|_| start_ms == end_ms || frame(start_ms) < frame(end_ms))
    }

    /// A line's times (ms) after the shift; `None` when it does not move.
    pub fn times(&self, start_ms: i64, end_ms: i64) -> Option<(i64, i64)> {
        let s = self.stretch(start_ms, end_ms)?;
        let at = |t: i64| {
            ms(self
                .grid
                .moved(i128::from(t) * 1_000_000, s.frames, self.precision))
        };
        Some((at(start_ms), at(end_ms)))
    }
}

/// What a timing fix comes to on its track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub outcome: Outcome,
    /// The blocks it moves, as (index, timestamp, duration) in TimestampScale units.
    pub times: Vec<(usize, i64, Option<u64>)>,
    /// Where its lines are, which says where the track's typos are.
    pub lines: Lines,
}

/// Where a timing fix's lines read on their track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lines {
    /// Where the scan found them: the fix is to do (or, once they were found so, left for a hand
    /// fix).
    AsScanned,
    /// Moved by the fix: it is in already.
    Moved,
    /// Neither, or not looked at.
    Unsure,
}

/// Where a recorded line is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    AsRecorded,
    Shifted,
    Missing,
}

/// Where each moving stretch's first and last line is on a track whose lines are `keys` (as
/// [`key`] gives them): found by its times and the fields before its text, so that a typo fixed in
/// it, in the same remux or an earlier one, leaves it found; at the recorded times, or, given the
/// shift, at those times shifted. An end the shift does not move (one shown on no frame) reads the
/// same either way and is not asked.
fn ends(
    fix: &TimingFix,
    keys: &[(&str, i64, i64)],
    kind: Kind,
    shifts: Option<&Shifts>,
) -> Vec<Found> {
    let known: HashSet<&(&str, i64, i64)> = keys.iter().collect();
    let find = |l: &LineRef| -> Found {
        let at = |start, end| known.contains(&key(kind, &l.raw, start, end));
        if at(l.start_ms, l.end_ms) {
            Found::AsRecorded
        } else if shifts
            .and_then(|s| s.times(l.start_ms, l.end_ms))
            .is_some_and(|(s, e)| at(s, e))
        {
            Found::Shifted
        } else {
            Found::Missing
        }
    };
    let moved = |l: &&LineRef| match shifts {
        Some(s) => s.stretch(l.start_ms, l.end_ms).is_some(),
        None => moves(fix, l.start_ms, l.end_ms),
    };
    fix.stretches
        .iter()
        .flat_map(|s| s.first.iter().chain(&s.last))
        .filter(moved)
        .map(find)
        .collect()
}

/// Where `fix`'s lines are on `track` with no frame grid to shift their times by: where the scan
/// found them, or not to be told.
pub fn lines_without_grid(fix: &TimingFix, track: &Track) -> Lines {
    let lines: Vec<(String, i64, i64)> = (0..track.blocks.len()).map(|i| line(track, i)).collect();
    let keys: Vec<(&str, i64, i64)> = lines
        .iter()
        .map(|(raw, s, e)| key(track.kind, raw, *s, *e))
        .collect();
    let found = ends(fix, &keys, track.kind, None);
    if !found.is_empty() && found.iter().all(|&f| f == Found::AsRecorded) {
        Lines::AsScanned
    } else {
        Lines::Unsure
    }
}

/// The fix on `track`: which blocks move and to where, or why none do. Each stretch's first and
/// last moved line says whether the fix is still to do (at the recorded times), in already (at
/// those times shifted), or the file changed (neither). Every time must move by exactly its
/// stretch's frames, so one a frame after a keyframe lands on it, or the fix is left for a hand
/// fix.
pub fn plan(fix: &TimingFix, track: &Track, grid: &Grid) -> Plan {
    let shifts = Shifts::new(fix, *grid, track.kind);
    let lines: Vec<(String, i64, i64)> = (0..track.blocks.len()).map(|i| line(track, i)).collect();
    let keys: Vec<(&str, i64, i64)> = lines
        .iter()
        .map(|(raw, s, e)| key(track.kind, raw, *s, *e))
        .collect();
    let checks = ends(fix, &keys, track.kind, Some(&shifts));
    let none = |outcome, lines| Plan {
        outcome,
        times: Vec::new(),
        lines,
    };
    if checks.is_empty() || checks.contains(&Found::Missing) {
        return none(Outcome::NotFound, Lines::Unsure);
    }
    if checks.iter().all(|&f| f == Found::Shifted) {
        return none(Outcome::AlreadyApplied, Lines::Moved);
    }
    if checks.contains(&Found::Shifted) {
        return none(Outcome::NotFound, Lines::Unsure);
    }
    let hand = |why: &str| none(Outcome::ByHand(why.into()), Lines::AsScanned);
    let mut times = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        let Some(s) = shifts.stretch(k.1, k.2) else {
            continue;
        };
        let (start, end) = (i128::from(k.1) * 1_000_000, i128::from(k.2) * 1_000_000);
        let moved = |x| grid.moved_surely(x, s.frames, shifts.precision);
        let (Some(new_start), Some(new_end)) = (moved(start), moved(end)) else {
            return hand("the keyframes do not fix the frame grid to the millisecond");
        };
        // Rounded to the track's precision, a time in a frame shorter than that may land in
        // another; one at the very start stays there.
        for (old, new) in [(start, new_start), (end, new_end)] {
            if new != 0 && grid.frame_of(new) != grid.frame_of(old) - s.frames {
                return hand("the shift did not land on the frame grid");
            }
        }
        // A line shown on a frame keeps one, but one at the very start shifted earlier, its start
        // held at 0, may have no time left.
        if new_end <= new_start && end > start {
            return hand("the shift leaves a line no time");
        }
        let scale = i128::from(track.scale.max(1));
        let timestamp = i64::try_from(new_start / scale).unwrap_or(i64::MAX);
        // Moved times keep their order; were one ever to come before the other, a hand fix
        // rather than a wrong line.
        let Ok(length) = u64::try_from((new_end - new_start) / scale) else {
            return hand("the shift leaves a line no time");
        };
        // The moved end, as a duration; a block with none and no DefaultDuration to take one from
        // keeps none (an SRT line shown until the next).
        let has_end = track.blocks[i].duration.is_some() || track.default_duration.is_some();
        times.push((i, timestamp, has_end.then_some(length)));
    }
    Plan {
        outcome: Outcome::Shifted {
            events: times.len(),
        },
        times,
        lines: Lines::AsScanned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::Block;

    /// 23.976 fps from 0, stored as K-On!! E09's frames are: frames 12022, 26180 and 30005 are
    /// keyframes, the last stored at 20:51.458 where plain rounding of 30005 frames of 1001/24 ms
    /// (1251458.54) gives 20:51.459.
    fn e09() -> Video {
        Video {
            frame_ns: 41_708_333,
            keyframes_ns: vec![0, 501_418_000_000, 1_091_924_000_000, 1_251_458_000_000],
        }
    }

    fn e09_grid() -> Grid {
        Grid::fit(&e09(), 1_000_000).unwrap()
    }

    const CS: i128 = 10_000_000;

    fn ms_of(ns: i128) -> i64 {
        ms(ns)
    }

    #[test]
    fn a_time_a_frame_late_moves_to_its_frame_before_at_the_centisecond() {
        let grid = e09_grid();
        // The cut at 18:11.924 is frame 26180; the sign starting at 18:11.95 shows from 26181.
        assert_eq!(grid.at(26_180), 1_091_924_000_000);
        assert_eq!(grid.frame_of(1_091_950_000_000), 26_181);
        // Moved a frame, it lands 16 ms before the cut, and shows from the cut.
        let moved = grid.moved(1_091_950_000_000, 1, CS);
        assert_eq!(ms_of(moved), 1_091_910);
        assert_eq!(grid.frame_of(moved), 26_180);
        assert_eq!(crate::timing::frame_offset(&e09(), ms_of(moved)), 0);
    }

    #[test]
    fn a_time_on_a_frame_lands_on_the_frame_before_and_rounding_stays_inside_it() {
        let grid = e09_grid();
        // Exactly frame 26181's time (18:11.966): to frame 26180, 18:11.924, written 18:11.92.
        assert_eq!(ms_of(grid.moved(grid.at(26_181), 1, CS)), 1_091_920);
        // Frame 26182's time moves to 26181's, 18:11.966, which rounds up past it to 18:11.97:
        // kept inside, it is 18:11.96.
        let moved = grid.moved(grid.at(26_182), 1, CS);
        assert_eq!(ms_of(moved), 1_091_960);
        assert_eq!(grid.frame_of(moved), 26_181);
    }

    #[test]
    fn two_frames_or_later_moves_as_far() {
        let grid = e09_grid();
        let x = 1_091_950_000_000;
        assert_eq!(grid.frame_of(grid.moved(x, 2, CS)), 26_179);
        assert_eq!(grid.frame_of(grid.moved(x, -1, CS)), 26_182);
        // At the millisecond (SRT) it keeps its place in the frame to the millisecond.
        assert_eq!(ms_of(grid.moved(x, 1, 1_000_000)), 1_091_908);
    }

    #[test]
    fn the_keyframes_fix_the_phase_of_the_frames() {
        let grid = e09_grid();
        assert_eq!(grid.at(30_005), 1_251_458_000_000);
        // A millisecond past that frame, a line starts on the next.
        assert_eq!(grid.frame_of(1_251_459_000_000), 30_006);
        // Frame 5, at 208.54 ms exactly, is stored at 208 ms, as in E09.
        assert_eq!(grid.at(5), 208_000_000);
        // Keyframes no one phase puts on frames: a cut 25 ms past a frame at 25 fps.
        let off = Video {
            frame_ns: 40_000_000,
            keyframes_ns: vec![0, 2_025_000_000, 6_025_000_000],
        };
        assert_eq!(
            Grid::fit(&off, 1_000_000).map(|_| ()),
            Err("the video's keyframes are on no one frame grid".into())
        );
    }

    #[test]
    fn a_long_film_is_on_the_exact_rate_not_the_default_duration() {
        // Two hours in, 41708333 ns a frame has drifted 67 µs from 1001/24 ms: no one phase fits
        // its keyframes.
        let mut film = e09();
        film.keyframes_ns
            .extend([8_342_251_000_000, 8_342_542_000_000]);
        let grid = Grid::fit(&film, 1_000_000).unwrap();
        assert_eq!(grid.at(200_021), 8_342_542_000_000);
    }

    #[test]
    fn a_time_at_the_very_start_stays_there() {
        let grid = e09_grid();
        assert_eq!(grid.moved(0, 1, CS), 0);
        // A frame in, it moves to the start.
        assert_eq!(grid.moved(grid.at(1), 1, CS), 0);
    }

    /// 25 fps, a keyframe every 2 s.
    fn grid() -> Grid {
        let video = Video {
            frame_ns: 40_000_000,
            keyframes_ns: (0..=100).map(|i| i * 2_000_000_000).collect(),
        };
        Grid::fit(&video, 1_000_000).unwrap()
    }

    fn block(order: usize, start_ms: i64, end_ms: i64, text: &str) -> Block {
        Block {
            track: 1,
            position: 0,
            timestamp: start_ms,
            duration: Some(u64::try_from(end_ms - start_ms).unwrap()),
            payload: format!("{order},0,Default,,0,0,0,,{text}").into_bytes(),
        }
    }

    fn line(start_ms: i64, end_ms: i64, text: &str) -> LineRef {
        LineRef {
            raw: format!("0,Default,,0,0,0,,{text}"),
            start_ms,
            end_ms,
        }
    }

    /// Three lines a frame late: each starts and ends a frame after a cut.
    fn late() -> Vec<Block> {
        vec![
            block(0, 2040, 4040, "One."),
            block(1, 6040, 8040, "Two."),
            block(2, 10_040, 12_040, "Three."),
        ]
    }

    fn stretch(
        from_ms: Option<i64>,
        to_ms: Option<i64>,
        frames: i64,
        lines: Option<(LineRef, LineRef)>,
    ) -> FixStretch {
        let (first, last) = lines.unzip();
        FixStretch {
            from_ms,
            to_ms,
            frames,
            first,
            last,
            frame_ms: None,
        }
    }

    fn fix(stretches: Vec<FixStretch>, unshifted: Vec<LineRef>) -> TimingFix {
        TimingFix {
            track: 1,
            frame_ns: 40_000_000,
            stretches,
            unshifted,
            basis: crate::store::Basis::Cuts {
                times: 6,
                agreeing: 1,
                windows: 1,
            },
        }
    }

    fn track(blocks: &[Block]) -> Track<'_> {
        Track {
            id: 1,
            kind: Kind::Ass,
            blocks,
            header: b"",
            scale: 1_000_000,
            default_duration: None,
            timing: crate::apply::patch::Timing::AsScanned,
            writable: Ok(()),
        }
    }

    fn whole() -> TimingFix {
        let ends = (line(2040, 4040, "One."), line(10_040, 12_040, "Three."));
        fix(vec![stretch(None, None, 1, Some(ends))], vec![])
    }

    #[test]
    fn a_late_track_moves_every_line_a_frame_and_then_reads_as_applied() {
        let blocks = late();
        let p = plan(&whole(), &track(&blocks), &grid());
        assert_eq!(p.outcome, Outcome::Shifted { events: 3 });
        assert_eq!(
            p.times,
            [
                (0, 2000, Some(2000)),
                (1, 6000, Some(2000)),
                (2, 10_000, Some(2000))
            ]
        );
        // The same track remuxed with those times, and a typo fixed in its last line: in already.
        let mut moved: Vec<Block> = blocks
            .iter()
            .zip(&p.times)
            .map(|(b, &(_, timestamp, duration))| Block {
                timestamp,
                duration,
                ..b.clone()
            })
            .collect();
        moved[2].payload = b"2,0,Default,,0,0,0,,Three!".to_vec();
        assert_eq!(
            plan(&whole(), &track(&moved), &grid()).outcome,
            Outcome::AlreadyApplied
        );
        // One end in and the other not: not this fix's doing.
        let mut half = blocks.clone();
        half[0] = moved[0].clone();
        assert_eq!(
            plan(&whole(), &track(&half), &grid()).outcome,
            Outcome::NotFound
        );
        // A track that reads neither way changed since the scan: a line's time, or its style.
        let mut other = blocks.clone();
        other[2].timestamp += 100;
        assert_eq!(
            plan(&whole(), &track(&other), &grid()).outcome,
            Outcome::NotFound
        );
        let mut other = blocks.clone();
        other[0].payload = b"0,0,Sign,,0,0,0,,One.".to_vec();
        assert_eq!(
            plan(&whole(), &track(&other), &grid()).outcome,
            Outcome::NotFound
        );
    }

    #[test]
    fn with_no_grid_a_fixs_lines_read_as_scanned_or_unsure() {
        let blocks = late();
        assert_eq!(
            lines_without_grid(&whole(), &track(&blocks)),
            Lines::AsScanned
        );
        // Moved, or changed: no grid to tell which.
        let mut moved = blocks.clone();
        moved[0].timestamp = 2000;
        assert_eq!(lines_without_grid(&whole(), &track(&moved)), Lines::Unsure);
    }

    #[test]
    fn a_partial_fix_moves_only_its_late_stretches_and_leaves_its_unshifted_lines() {
        let blocks = late();
        let ends = (line(2040, 4040, "One."), line(6040, 8040, "Two."));
        let partial = fix(
            vec![
                stretch(None, Some(8040), 1, Some(ends)),
                stretch(Some(10_040), None, 0, None),
            ],
            vec![],
        );
        let p = plan(&partial, &track(&blocks), &grid());
        let moved: Vec<usize> = p.times.iter().map(|t| t.0).collect();
        assert_eq!(
            (p.outcome, moved),
            (Outcome::Shifted { events: 2 }, vec![0, 1])
        );
        // Line two recorded as left where it is.
        let ends = (line(2040, 4040, "One."), line(2040, 4040, "One."));
        let with_unshifted = fix(
            vec![
                stretch(None, Some(8040), 1, Some(ends)),
                stretch(Some(10_040), None, 0, None),
            ],
            vec![line(6040, 8040, "Two.")],
        );
        let p = plan(&with_unshifted, &track(&blocks), &grid());
        assert_eq!(p.times.iter().map(|t| t.0).collect::<Vec<_>>(), [0]);
        // Still so with a typo in it fixed by an earlier remux.
        let mut fixed = blocks.clone();
        fixed[1].payload = b"1,0,Default,,0,0,0,,Two!".to_vec();
        let p = plan(&with_unshifted, &track(&fixed), &grid());
        assert_eq!(p.times.iter().map(|t| t.0).collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn a_shift_that_misses_the_frame_grid_is_left_for_a_hand_fix() {
        // At 120 fps frame 7 (50 to 58 ms) holds no centisecond: a line starting in frame 8, at
        // 60 ms, cannot be written a frame earlier.
        let video = Video {
            frame_ns: 8_333_333,
            keyframes_ns: (0..=10).map(|i| i * 2_000_000_000).collect(),
        };
        let fast = Grid::fit(&video, 1_000_000).unwrap();
        let blocks = [block(0, 60, 2000, "One.")];
        let ends = (line(60, 2000, "One."), line(60, 2000, "One."));
        let fix = fix(vec![stretch(None, None, 1, Some(ends))], vec![]);
        assert_eq!(
            plan(&fix, &track(&blocks), &fast).outcome,
            Outcome::ByHand("the shift did not land on the frame grid".into())
        );
    }

    #[test]
    fn keyframes_that_leave_a_frame_a_millisecond_unsure_leave_its_times_for_a_hand_fix() {
        // A fixed GOP: a keyframe every 24 frames at 23.976 fps, each on a whole millisecond, so
        // any phase within half a millisecond fits, and frame 5 (208.54 ms) is at 208 or 209.
        let video = Video {
            frame_ns: 41_708_333,
            keyframes_ns: (0..=40).map(|k| k * 1_001_000_000).collect(),
        };
        let gop = Grid::fit(&video, 1_000_000).unwrap();
        let whole = |start| {
            let blocks = [block(0, start, 2000, "One.")];
            let ends = (line(start, 2000, "One."), line(start, 2000, "One."));
            let fix = fix(vec![stretch(None, None, 1, Some(ends))], vec![]);
            plan(&fix, &track(&blocks), &gop).outcome
        };
        // At 209 ms the line starts on frame 5 or 6; at 230 ms on 6 either way.
        assert_eq!(
            whole(209),
            Outcome::ByHand("the keyframes do not fix the frame grid to the millisecond".into())
        );
        // Frame 2 (83.42 ms) is at 84 only for phases above the middle: at 84 ms a line starts
        // on frame 2 or 3.
        assert_eq!(
            whole(84),
            Outcome::ByHand("the keyframes do not fix the frame grid to the millisecond".into())
        );
        assert_eq!(whole(230), Outcome::Shifted { events: 1 });
    }

    #[test]
    fn a_line_timed_by_the_default_duration_reads_as_in_after_its_shift() {
        // 41.708 ms of DefaultDuration from 2039 ms: the scan records its end at 2080 ms, on the
        // frame; moved as that (not as 2080.708, a frame later), the re-run finds it.
        let blocks = [Block {
            duration: None,
            ..block(0, 2039, 2039, "One.")
        }];
        let t = Track {
            default_duration: Some(41_708_333),
            ..track(&blocks)
        };
        let ends = (line(2039, 2080, "One."), line(2039, 2080, "One."));
        let fix = fix(vec![stretch(None, None, 1, Some(ends))], vec![]);
        let p = plan(&fix, &t, &grid());
        assert_eq!(p.times, [(0, 2000, Some(40))]);
        let moved = [Block {
            timestamp: 2000,
            duration: Some(40),
            ..blocks[0].clone()
        }];
        let again = plan(
            &fix,
            &Track {
                blocks: &moved,
                ..t
            },
            &grid(),
        );
        assert_eq!(again.outcome, Outcome::AlreadyApplied);
    }

    #[test]
    fn a_line_the_shift_leaves_no_time_is_left_for_a_hand_fix() {
        // One frame from 0: a frame earlier, it would start and end at 0.
        let blocks = [block(0, 0, 40, "One.")];
        let ends = (line(0, 40, "One."), line(0, 40, "One."));
        let fix = fix(vec![stretch(None, None, 1, Some(ends))], vec![]);
        assert_eq!(
            plan(&fix, &track(&blocks), &grid()).outcome,
            Outcome::ByHand("the shift leaves a line no time".into())
        );
    }

    #[test]
    fn a_line_shown_on_no_frame_stays_where_it_is() {
        // 4 ms at the end of a frame (2036 to 2040 ms), first in a stretch a frame late: the
        // others move, it stays, in ASS or SRT alike.
        let blocks = [
            block(0, 2036, 2040, "Blink."),
            block(1, 2040, 3000, "One."),
            block(2, 4040, 5000, "Two."),
        ];
        let ends = (line(2036, 2040, "Blink."), line(4040, 5000, "Two."));
        let fix = fix(vec![stretch(None, None, 1, Some(ends))], vec![]);
        for kind in [Kind::Ass, Kind::Srt] {
            let t = Track {
                kind,
                ..track(&blocks)
            };
            let p = plan(&fix, &t, &grid());
            assert_eq!(p.times, [(1, 2000, Some(960)), (2, 4000, Some(960))]);
            // Remuxed so, it reads as in: the unmoved first line, the same either way, is not
            // what tells.
            let mut moved = blocks.clone();
            for &(i, timestamp, duration) in &p.times {
                moved[i] = Block {
                    timestamp,
                    duration,
                    ..blocks[i].clone()
                };
            }
            let again = Track {
                blocks: &moved,
                ..t
            };
            assert_eq!(plan(&fix, &again, &grid()).outcome, Outcome::AlreadyApplied);
        }
    }

    #[test]
    fn a_block_without_a_duration_gets_the_moved_one_of_the_tracks_or_keeps_none() {
        // Its end, from the track's DefaultDuration, moves as an end: its own from now on.
        let blocks: Vec<Block> = late()
            .into_iter()
            .map(|b| Block {
                duration: None,
                ..b
            })
            .collect();
        let t = Track {
            default_duration: Some(2_000_000_000),
            ..track(&blocks)
        };
        let p = plan(&whole(), &t, &grid());
        let moved = [
            (0, 2000, Some(2000)),
            (1, 6000, Some(2000)),
            (2, 10_000, Some(2000)),
        ];
        assert_eq!(p.times, moved);
        // With no DefaultDuration either (an SRT line shown until the next), it keeps none.
        let blocks = [Block {
            duration: None,
            ..block(0, 2040, 2040, "One.")
        }];
        let ends = (line(2040, 2040, "One."), line(2040, 2040, "One."));
        let fix = fix(vec![stretch(None, None, 1, Some(ends))], vec![]);
        assert_eq!(
            plan(&fix, &track(&blocks), &grid()).times,
            [(0, 2000, None)]
        );
    }

    #[test]
    fn a_typos_times_shift_with_the_stretch_they_fall_in() {
        let ends = (line(2040, 4040, "One."), line(6040, 8040, "Two."));
        let partial = fix(
            vec![
                stretch(None, Some(8040), 1, Some(ends)),
                stretch(Some(10_040), None, 0, None),
            ],
            vec![],
        );
        let shifts = Shifts::new(&partial, grid(), Kind::Ass);
        assert_eq!(shifts.times(6040, 8040), Some((6000, 8000)));
        assert_eq!(shifts.times(10_040, 12_040), None);
        // A line left where it is stays at its times.
        let kept = fix(partial.stretches.clone(), vec![line(6040, 8040, "Two.")]);
        let shifts = Shifts::new(&kept, grid(), Kind::Ass);
        assert_eq!(shifts.times(6040, 8040), None);
        assert_eq!(shifts.times(2040, 4040), Some((2000, 4000)));
    }
}
