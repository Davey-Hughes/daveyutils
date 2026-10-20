//! `subcheck timing`: whether a subtitle track is shifted by whole frames against the video.
//!
//! A line timed to the picture starts and stops on cuts, and the video's keyframes stand in for
//! the cuts. Counting the frames between each line's start or end and the nearest keyframe piles a
//! timed style up at one offset: 0 when in sync, +1 when every line is a frame late. Pure functions
//! over plain data; `main` reads the file.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write;

use crate::mkv::{Layout, TRACK_TYPE_VIDEO};
use crate::text::{self, Event};

pub mod lines;
pub mod record;
pub mod shared;

/// The version of these rules, kept with each file `scan` checks: a file checked by other rules is
/// checked again. Bump it when a rule or threshold changes what a track's verdict would be.
pub const RULES_VERSION: u32 = 1;

/// Fewer keyframe Cues than this and there is nothing to measure against.
const MIN_KEYFRAMES: usize = 10;

/// Cues sparser than one per this many nanoseconds of the span they cover do not index every
/// keyframe: x264's default puts one at least every 250 frames (10.4 s at 23.976 fps), and K-On's
/// encodes one every 24-26 frames.
const MAX_KEYFRAME_GAP_NS: u64 = 10_000_000_000;

/// Cues denser than one per this many frames, on average, are not cuts. An index of every frame
/// (all-intra video, or a mux with `--cues 0:all`) averages 1, or 2 when DefaultDuration is a field,
/// and would put every time at offset 0. K-On's keyframes are 24-26 frames apart.
const MIN_KEYFRAME_GAP_FRAMES: u64 = 4;

/// Slack for times rounded to the millisecond: a frame at 1091.9243 s is stored as 1091.924 s.
const SLACK_NS: i128 = 500_000;

/// Offsets from -MAX_OFFSET to +MAX_OFFSET frames are counted; a time further from every keyframe
/// is nowhere near a cut.
pub const MAX_OFFSET: i64 = 3;
/// How many offsets that is.
pub const OFFSETS: usize = 7;

/// A style with at least this many times is timed when its top two offsets hold at least
/// TOP_TWO_PERCENT of them. E09: dialogue keeps 54-68% of its times in its top two offsets,
/// karaoke and effects about 30% (flat over seven offsets is 29%).
const MANY_TIMES: usize = 16;
const TOP_TWO_PERCENT: usize = 45;
/// A style with 2 to 15 times is timed when its top offset holds at least this share.
const FEW_TOP_PERCENT: usize = 50;

/// The timed styles' times are cut into windows this long, so a shift over part of a track shows:
/// E16's dialogue has 108 times at +1 and 72 at 0 over a background of ~30, which no whole-track
/// threshold tells from noise, and in 3-minute windows reads `+1 +1 ? 0 0 ? +1 +1`.
const WINDOW_MS: i64 = 180_000;
/// An offset decides a window when at least this many times sit there, and twice as many as at any
/// other offset; with no window decided, the same goes for the whole track. On the 36 K-On
/// episodes, dialogue windows hold 11-53 times at their offset, while an OP's lyrics, timed to the
/// beat a few frames off the cuts, hold 4-8 (K-On! E01: 8 at +2, K-On!! E01: 4 at -3), and so does
/// the ED closing K-On!! E17 (4 at -3). A bar of 4 let those decide a window, and print a fix.
const MIN_AT_OFFSET: usize = 10;

/// What the video says about frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Video {
    /// The video track's DefaultDuration.
    pub frame_ns: u64,
    /// Sorted, distinct.
    pub keyframes_ns: Vec<u64>,
}

/// The first video track's frame duration and keyframes (its Cues), or why they cannot stand in
/// for cuts.
pub fn video(layout: &Layout) -> Result<Video, String> {
    let track = layout
        .tracks
        .iter()
        .find(|t| t.kind == TRACK_TYPE_VIDEO)
        .ok_or_else(|| "no video track".to_string())?;
    let frame_ns = track
        .default_duration
        .filter(|&d| d > 0)
        .ok_or_else(|| "no DefaultDuration on the video track".to_string())?;
    let mut keyframes_ns: Vec<u64> = layout
        .cues
        .iter()
        .filter(|c| c.track == track.number)
        .map(|c| c.time.saturating_mul(layout.timestamp_scale))
        .collect();
    keyframes_ns.sort_unstable();
    keyframes_ns.dedup();
    let n = keyframes_ns.len();
    if n < MIN_KEYFRAMES {
        return Err(format!("only {n} video keyframes in the Cues"));
    }
    let span = keyframes_ns[n - 1] - keyframes_ns[0];
    let over = || text::timestamp(i64::try_from(span / 1_000_000).unwrap_or(i64::MAX));
    if (n as u64).saturating_mul(MAX_KEYFRAME_GAP_NS) < span {
        return Err(format!(
            "{n} video keyframes in the Cues over {}: fewer than one per {} s",
            over(),
            MAX_KEYFRAME_GAP_NS / 1_000_000_000
        ));
    }
    if span < (n as u64 - 1).saturating_mul(MIN_KEYFRAME_GAP_FRAMES.saturating_mul(frame_ns)) {
        return Err(format!(
            "{n} video keyframes in the Cues over {}: more than one per {MIN_KEYFRAME_GAP_FRAMES} frames",
            over()
        ));
    }
    Ok(Video {
        frame_ns,
        keyframes_ns,
    })
}

/// The frame on which a line starting or ending at `time_ms` starts or stops showing, counted from
/// the nearest keyframe: 0 on the keyframe, +1 one frame after it. mpv shows a line on a frame when
/// `start <= frame time < end`, so that frame is the first one at or after the time. An i128, as
/// [`frame_index`].
pub fn frame_offset(video: &Video, time_ms: i64) -> i128 {
    frames_from(video, time_ms, nearest_keyframe(video, time_ms))
}

/// The frame a line starting or ending at `time_ms` starts or stops showing on, counted from the
/// one at `origin_ns`.
fn frames_from(video: &Video, time_ms: i64, origin_ns: i128) -> i128 {
    // ceil(d / frame), for a positive frame duration.
    let d = i128::from(time_ms) * 1_000_000 - origin_ns - SLACK_NS;
    -(-d).div_euclid(i128::from(video.frame_ns))
}

/// An offset or gap in frames, if it is at most MAX_OFFSET either way.
fn within(frames: i128) -> Option<i64> {
    i64::try_from(frames)
        .ok()
        .filter(|f| (-MAX_OFFSET..=MAX_OFFSET).contains(f))
}

/// The keyframe nearest `time_ms`, in nanoseconds.
fn nearest_keyframe(video: &Video, time_ms: i64) -> i128 {
    let x = i128::from(time_ms) * 1_000_000;
    let after = video.keyframes_ns.partition_point(|&k| i128::from(k) < x);
    [after.checked_sub(1), Some(after)]
        .into_iter()
        .flatten()
        .filter_map(|i| video.keyframes_ns.get(i))
        .map(|&k| i128::from(k))
        .min_by_key(|k| (k - x).abs())
        .expect("video() keeps at least MIN_KEYFRAMES keyframes")
}

/// The frame on which a line starting or ending at `time_ms` starts or stops showing, counted on
/// the video's frame grid from its first keyframe. Two times on one frame have the same index. An
/// i128: at a short enough frame, a time read from a corrupt file is more frames in than an i64
/// counts.
pub fn frame_index(video: &Video, time_ms: i64) -> i128 {
    let first = video.keyframes_ns.first().map_or(0, |&k| i128::from(k));
    frames_from(video, time_ms, first)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    InSync,
    /// Every line late (positive) or early (negative) by this many frames.
    Shifted(i64),
    /// Stretches at different offsets, in time order.
    Partial(Vec<Stretch>),
    /// Under 16 times, and they decide nothing.
    TooFew,
    /// 16 or more, and no offset stands out.
    Unclear,
}

/// Part of a track at one offset. Each end is a time of the track's own, so the range can be
/// selected by time in Aegisub; `None` is the start or the end of the track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stretch {
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub frames: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    pub verdict: Verdict,
    /// The timed styles' times.
    pub times: usize,
    /// How many of them sit at each offset.
    pub pool: [usize; OFFSETS],
    /// Windows from 0 to the last time, decided or not.
    pub windows: usize,
    /// Windows at a whole-track verdict's offset; 0 when the pool decided it.
    pub agreeing: usize,
    pub styles: Vec<StyleCounts>,
    /// The video's, for the fix.
    pub frame_ns: u64,
    /// Each event's stretch, an index into the verdict's stretches (a shifted or in-sync track is
    /// one stretch, 0); `None` for an event that stays where it is, and for every event when no
    /// offset was decided.
    pub each: Vec<Option<usize>>,
    /// A partial track's blocks of lines that stay where they are, (start, end) in ms: their times
    /// point both ways.
    pub unshifted: Vec<(i64, i64)>,
}

/// One track's events against the video.
pub fn analyse(video: &Video, events: &[Event]) -> Analysis {
    let (styles, pool) = styles(video, events);
    let counts = count(&pool);
    let windows = windows(&pool);
    let runs = runs(&windows);
    let (verdict, agreeing) = match runs.as_slice() {
        [] => {
            let verdict = match decided(&counts) {
                Some(offset) => whole(offset),
                None if pool.len() < MANY_TIMES => Verdict::TooFew,
                None => Verdict::Unclear,
            };
            (verdict, 0)
        }
        [one] => (whole(one.offset), one.windows),
        many => (Verdict::Partial(stretches(&pool, many)), 0),
    };
    let (verdict, each, unshifted) = match verdict {
        Verdict::Partial(pooled) => {
            let m = lines::line_exact(video, events, &pooled);
            (Verdict::Partial(m.stretches), m.each, m.unshifted)
        }
        v @ (Verdict::InSync | Verdict::Shifted(_)) => (v, vec![Some(0); events.len()], Vec::new()),
        v => (v, vec![None; events.len()], Vec::new()),
    };
    Analysis {
        verdict,
        times: pool.len(),
        pool: counts,
        windows: windows.len(),
        agreeing,
        styles,
        frame_ns: video.frame_ns,
        each,
        unshifted,
    }
}

fn whole(offset: i64) -> Verdict {
    if offset == 0 {
        Verdict::InSync
    } else {
        Verdict::Shifted(offset)
    }
}

/// The offset most of `counts` sit at, if it has at least MIN_AT_OFFSET and twice the runner-up.
fn decided(counts: &[usize; OFFSETS]) -> Option<i64> {
    let (top, &n) = counts.iter().enumerate().max_by_key(|&(_, &n)| n)?;
    let runner_up = counts
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != top)
        .map(|(_, &n)| n)
        .max()
        .unwrap_or(0);
    (n >= MIN_AT_OFFSET && n >= 2 * runner_up).then(|| offset_at(top))
}

fn window_of(ms: i64) -> usize {
    usize::try_from(ms.max(0) / WINDOW_MS).unwrap_or(0)
}

fn window_start(window: usize) -> i64 {
    i64::try_from(window).unwrap_or(i64::MAX / WINDOW_MS) * WINDOW_MS
}

/// Each window's offset, where one is decided, from 0 to the window of the last time.
fn windows(pool: &[Time]) -> Vec<Option<i64>> {
    let Some(last) = pool.last() else {
        return Vec::new();
    };
    let mut counts = vec![[0; OFFSETS]; window_of(last.ms) + 1];
    for t in pool {
        counts[window_of(t.ms)][slot(t.offset)] += 1;
    }
    counts.iter().map(decided).collect()
}

/// Consecutive decided windows at one offset; undecided windows between them are skipped over.
struct Run {
    offset: i64,
    first: usize,
    last: usize,
    windows: usize,
}

fn runs(windows: &[Option<i64>]) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for (w, offset) in windows.iter().enumerate() {
        let Some(offset) = *offset else {
            continue;
        };
        match out.last_mut() {
            Some(run) if run.offset == offset => {
                run.last = w;
                run.windows += 1;
            }
            _ => out.push(Run {
                offset,
                first: w,
                last: w,
                windows: 1,
            }),
        }
    }
    out
}

fn stretches(pool: &[Time], runs: &[Run]) -> Vec<Stretch> {
    let mut out: Vec<Stretch> = runs
        .iter()
        .map(|r| Stretch {
            from_ms: None,
            to_ms: None,
            frames: r.offset,
        })
        .collect();
    for (i, pair) in runs.windows(2).enumerate() {
        let (to, from) = change_point(pool, &pair[0], &pair[1]);
        out[i].to_ms = Some(to);
        out[i + 1].from_ms = Some(from);
    }
    out
}

/// Where run `a`'s offset gives way to run `b`'s. Of the times at either offset from the start of
/// a's last window to the end of b's first, the split that leaves the most at a's offset before it
/// plus the most at b's after it (the earliest such split). Returns the times on either side.
fn change_point(pool: &[Time], a: &Run, b: &Run) -> (i64, i64) {
    let (lo, hi) = (window_start(a.last), window_start(b.first + 1));
    let near: Vec<&Time> = pool
        .iter()
        .filter(|t| t.ms >= lo && t.ms < hi && (t.offset == a.offset || t.offset == b.offset))
        .collect();
    // The score of splitting before near[k]: at a before it, plus at b from it on.
    let mut score = near.iter().filter(|t| t.offset == b.offset).count();
    let mut best: Option<(usize, usize)> = None;
    for k in 1..near.len() {
        if near[k - 1].offset == a.offset {
            score += 1;
        } else {
            score -= 1;
        }
        if near[k - 1].ms < near[k].ms && best.is_none_or(|(s, _)| score > s) {
            best = Some((score, k));
        }
    }
    // a's last window holds times at a, and all of them come before b's first window's times at b.
    let (_, k) = best.expect("a's last window comes before b's first");
    (near[k - 1].ms, near[k].ms)
}

/// One line start or end, in ms, and its offset in frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Time {
    pub ms: i64,
    pub offset: i64,
}

/// How many of one style's times sit at each offset, -3 to +3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleCounts {
    pub style: String,
    pub counts: [usize; OFFSETS],
    /// The style keeps to the cuts, so its times count.
    pub timed: bool,
}

/// Each style's counts, by name, and the timed styles' times pooled in time order. A style's
/// distinct starts and distinct ends are one time each; karaoke events are left out.
pub fn styles(video: &Video, events: &[Event]) -> (Vec<StyleCounts>, Vec<Time>) {
    let mut seen = HashSet::new();
    let mut by_style: BTreeMap<&str, Vec<Time>> = BTreeMap::new();
    for e in events.iter().filter(|e| !e.karaoke) {
        for (is_end, ms) in [(false, e.start_ms), (true, e.end_ms)] {
            if !seen.insert((e.style.as_str(), is_end, ms)) {
                continue;
            }
            if let Some(offset) = within(frame_offset(video, ms)) {
                by_style
                    .entry(e.style.as_str())
                    .or_default()
                    .push(Time { ms, offset });
            }
        }
    }
    let mut out = Vec::new();
    let mut pool = Vec::new();
    for (style, times) in by_style {
        let counts = count(&times);
        let timed = is_timed(&counts);
        if timed {
            pool.extend(times);
        }
        out.push(StyleCounts {
            style: style.to_string(),
            counts,
            timed,
        });
    }
    pool.sort_unstable();
    (out, pool)
}

/// How many of `times` sit at each offset.
fn count(times: &[Time]) -> [usize; OFFSETS] {
    let mut counts = [0; OFFSETS];
    for t in times {
        counts[slot(t.offset)] += 1;
    }
    counts
}

/// Where an offset, -3 to +3, is counted.
fn slot(offset: i64) -> usize {
    usize::try_from(offset + MAX_OFFSET).expect("offsets are within ±MAX_OFFSET")
}

/// The offset counted at `slot`.
pub fn offset_at(slot: usize) -> i64 {
    i64::try_from(slot).expect("slot < OFFSETS") - MAX_OFFSET
}

fn is_timed(counts: &[usize; OFFSETS]) -> bool {
    let total: usize = counts.iter().sum();
    let mut sorted = *counts;
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    if total >= MANY_TIMES {
        (sorted[0] + sorted[1]) * 100 >= total * TOP_TWO_PERCENT
    } else {
        total >= 2 && sorted[0] * 100 >= total * FEW_TOP_PERCENT
    }
}

/// One subtitle track's result, as [`render`] prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackReport {
    /// mkvmerge's track ID.
    pub id: usize,
    pub name: String,
    /// Or what to print in place of a verdict: `unsupported: <reason>`, `cannot read: <error>`.
    pub result: Result<Analysis, String>,
    /// What its lines shared with another track say.
    pub lines: Option<shared::Lines>,
}

/// One file's block: its name, a line per track, the fix or the stretches under a shifted or
/// partial track, and with `verbose` each style's counts. Columns line up within the block.
pub fn render(file: &str, tracks: &[TrackReport], verbose: bool) -> String {
    let mut out = format!("{file}\n");
    if tracks.is_empty() {
        out.push_str("  no text subtitle tracks to check\n");
        return out;
    }
    let id_width = tracks
        .iter()
        .map(|t| t.id.to_string().len())
        .max()
        .unwrap_or(1);
    let name_width = tracks
        .iter()
        .map(|t| t.name.chars().count())
        .max()
        .unwrap_or(0);
    let indent = " ".repeat("  track ".len() + id_width + 2);
    for t in tracks {
        let (verdict, detail) = match &t.result {
            Ok(a) => (verdict_label(&a.verdict), detail(a)),
            Err(line) => (line.clone(), None),
        };
        let line = format!(
            "  track {:<id_width$}  {:<name_width$}  {verdict:<16} {}",
            t.id,
            t.name,
            detail.unwrap_or_default()
        );
        out.push_str(line.trim_end());
        out.push('\n');
        let Ok(a) = &t.result else {
            continue;
        };
        out.push_str(&fix(&indent, t.id, &a.verdict, a.frame_ns, &a.unshifted));
        match &t.lines {
            Some(shared::Lines::Follows(f)) => {
                let _ = writeln!(
                    out,
                    "{indent}timed {} ({} of {} shared times): {}",
                    relation(f.gap, f.source),
                    f.shared.at(f.gap),
                    f.shared.times,
                    verdict_label(&f.verdict)
                );
                out.push_str(&fix(&indent, t.id, &f.verdict, a.frame_ns, &f.unshifted));
            }
            Some(shared::Lines::Disagrees { source, says }) => {
                let _ = writeln!(
                    out,
                    "{indent}but its lines shared with track {source} say {}",
                    verdict_label(says)
                );
            }
            None => {}
        }
        if verbose {
            out.push_str(&style_table(&indent, &a.styles));
        }
    }
    out
}

/// Under a shifted or partial verdict: the `mkvmerge --sync` fix, or the stretches.
fn fix(indent: &str, id: usize, v: &Verdict, frame_ns: u64, unshifted: &[(i64, i64)]) -> String {
    match v {
        Verdict::Shifted(frames) => {
            format!(
                "{indent}fix: mkvmerge --sync {id}:{}\n",
                sync_ms(*frames, frame_ns)
            )
        }
        Verdict::Partial(stretches) => ranges(indent, stretches, unshifted),
        _ => String::new(),
    }
}

/// "as track 5", "1 frame after track 5", "2 frames before track 5".
fn relation(gap: i64, source: usize) -> String {
    if gap == 0 {
        return format!("as track {source}");
    }
    format!("{} track {source}", frames_way(gap, "after", "before"))
}

/// "in sync", "1 frame late", "2 frames early".
pub fn shift_label(frames: i64) -> String {
    if frames == 0 {
        return "in sync".into();
    }
    frames_way(frames, "late", "early")
}

/// "1 frame late", "2 frames early": `frames` one way or the other, in the words given for more
/// (positive) and fewer (negative).
fn frames_way(frames: i64, more: &str, fewer: &str) -> String {
    let way = if frames > 0 { more } else { fewer };
    format!("{} {way}", frame_count(frames))
}

/// "1 frame", "2 frames": how many, whichever way.
pub fn frame_count(frames: i64) -> String {
    let n = frames.abs();
    let unit = if n == 1 { "frame" } else { "frames" };
    format!("{n} {unit}")
}

fn verdict_label(v: &Verdict) -> String {
    match v {
        Verdict::InSync => shift_label(0),
        Verdict::Shifted(frames) => shift_label(*frames),
        Verdict::Partial(_) => "partial".into(),
        Verdict::TooFew => "too few lines".into(),
        Verdict::Unclear => "unclear".into(),
    }
}

/// "+1", "0", "-2".
fn signed(offset: i64) -> String {
    if offset == 0 {
        "0".into()
    } else {
        format!("{offset:+}")
    }
}

/// What a verdict rests on: the windows that agree, or the pool when no window decided.
fn detail(a: &Analysis) -> Option<String> {
    let offset = match a.verdict {
        Verdict::InSync => 0,
        Verdict::Shifted(frames) => frames,
        Verdict::Partial(_) => return None,
        Verdict::TooFew => return Some(format!("({} times)", a.times)),
        Verdict::Unclear => return Some(format!("({} times, no offset stands out)", a.times)),
    };
    Some(if a.agreeing > 0 {
        format!(
            "({} times, {} in {} of {} windows)",
            a.times,
            signed(offset),
            a.agreeing,
            a.windows
        )
    } else {
        format!(
            "({} times, {} of them at {})",
            a.times,
            a.pool[slot(offset)],
            signed(offset)
        )
    })
}

/// A partial track's stretches, one per line, `1 frame late  start      – 0:07:55.78`, then the
/// blocks of lines that stay where they are.
fn ranges(indent: &str, stretches: &[Stretch], unshifted: &[(i64, i64)]) -> String {
    let mut rows: Vec<(String, String, String)> = stretches
        .iter()
        .map(|s| {
            (
                shift_label(s.frames),
                s.from_ms.map_or_else(|| "start".into(), text::timestamp),
                s.to_ms.map_or_else(|| "end".into(), text::timestamp),
            )
        })
        .collect();
    rows.extend(unshifted.iter().map(|&(from, to)| {
        (
            "not shifted".to_string(),
            text::timestamp(from),
            text::timestamp(to),
        )
    }));
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (label, from, to) in rows {
        let _ = writeln!(out, "{indent}{label:<width$}  {from:<10} – {to}");
    }
    out
}

/// Each style's count at each offset, and whether its times were used.
fn style_table(indent: &str, styles: &[StyleCounts]) -> String {
    let name = |s: &StyleCounts| {
        if s.style.is_empty() {
            "(none)".to_string()
        } else {
            s.style.clone()
        }
    };
    let width = styles
        .iter()
        .map(|s| name(s).chars().count())
        .chain(["style".len()])
        .max()
        .unwrap_or(0);
    let mut out = format!("{indent}{:<width$}", "style");
    for slot in 0..OFFSETS {
        let _ = write!(out, " {:>4}", signed(offset_at(slot)));
    }
    out.push('\n');
    for s in styles {
        let _ = write!(out, "{indent}{:<width$}", name(s));
        for n in s.counts {
            let _ = write!(out, " {n:>4}");
        }
        let _ = writeln!(out, "  {}", if s.timed { "used" } else { "ignored" });
    }
    out
}

/// The `mkvmerge --sync` delay, in ms, that undoes a shift of `frames` frames: -42 for one frame
/// late at 23.976 fps, -83 for two.
pub fn sync_ms(frames: i64, frame_ns: u64) -> i64 {
    let ns = i128::from(frames) * i128::from(frame_ns);
    // Rounded half away from zero.
    let ms = (ns.abs() + 500_000) / 1_000_000 * ns.signum();
    -i64::try_from(ms).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mkv::{Cue, TRACK_TYPE_SUBTITLE, Track};

    /// 23.976 fps, as mkvmerge stores it.
    const FRAME_NS: u64 = 41_708_333;

    /// E09 18:14: the shot the motion-tracked sign belongs to, and the next one.
    fn e09() -> Video {
        Video {
            frame_ns: FRAME_NS,
            keyframes_ns: vec![1_091_924_000_000, 1_094_927_000_000],
        }
    }

    #[test]
    fn the_e09_sign_starts_and_ends_a_frame_late() {
        assert_eq!(frame_offset(&e09(), 1_091_950), 1);
        assert_eq!(frame_offset(&e09(), 1_094_950), 1);
    }

    #[test]
    fn both_ways_of_ending_on_a_cut_are_in_sync() {
        // Aegisub's midpoint, half a frame before the cut, and the cut's own time.
        assert_eq!(frame_offset(&e09(), 1_094_906), 0);
        assert_eq!(frame_offset(&e09(), 1_094_927), 0);
        assert_eq!(frame_offset(&e09(), 1_094_928), 1);
    }

    #[test]
    fn whole_frames_late_or_early_count_as_such() {
        assert_eq!(frame_offset(&e09(), 1_094_927 + 83), 2);
        assert_eq!(frame_offset(&e09(), 1_094_906 + 83), 2);
        assert_eq!(frame_offset(&e09(), 1_094_927 - 42), -1);
    }

    #[test]
    fn a_frame_time_rounded_up_to_the_millisecond_stays_on_its_frame() {
        // The frame after the cut is at 1094968.708 ms; stored rounded, it is 1094969.
        assert_eq!(frame_offset(&e09(), 1_094_969), 1);
    }

    #[test]
    fn the_nearest_keyframe_is_the_one_measured_from() {
        // 1093.5 s is nearer the second keyframe (1.427 s away) than the first (1.576 s).
        assert_eq!(frame_offset(&e09(), 1_093_500), -34);
    }

    #[test]
    fn the_sync_fix_rounds_to_the_millisecond() {
        assert_eq!(sync_ms(1, FRAME_NS), -42);
        assert_eq!(sync_ms(2, FRAME_NS), -83);
        assert_eq!(sync_ms(-1, FRAME_NS), 42);
        assert_eq!(sync_ms(1, 500_000_000), -500);
    }

    /// 25 fps, a keyframe every 2 s for 30 minutes.
    fn video_2s() -> Video {
        Video {
            frame_ns: 40_000_000,
            keyframes_ns: (0..=900).map(|i| i * 2_000_000_000).collect(),
        }
    }

    /// `frames` frames after keyframe `n` of [`video_2s`], in ms.
    fn at(n: i64, frames: i64) -> i64 {
        n * 2000 + frames * 40
    }

    fn event(style: &str, start_ms: i64, end_ms: i64) -> Event {
        Event {
            track: 0,
            start_ms,
            end_ms,
            layer: "0".into(),
            style: style.into(),
            text: "Line.".into(),
            pos: None,
            karaoke: false,
            raw: format!("0,{style},,0,0,0,,Line."),
        }
    }

    #[test]
    fn each_distinct_start_and_end_of_a_style_is_one_time() {
        let events = [
            // Two rows of one line: one start, one end.
            event("D", at(1, 1), at(2, 1)),
            event("D", at(1, 1), at(2, 1)),
            // Starting where the line before ends: that is a start and an end, two times.
            event("D", at(2, 1), at(3, 0)),
            // The same times in another style count there too.
            event("S", at(1, 1), at(2, 1)),
        ];
        let (styles, _) = styles(&video_2s(), &events);
        let counts: Vec<(&str, [usize; OFFSETS])> = styles
            .iter()
            .map(|s| (s.style.as_str(), s.counts))
            .collect();
        assert_eq!(
            counts,
            [("D", [0, 0, 0, 1, 3, 0, 0]), ("S", [0, 0, 0, 0, 2, 0, 0])]
        );
    }

    #[test]
    fn karaoke_and_times_far_from_every_keyframe_are_left_out() {
        let mut song = event("OP", at(1, 0), at(2, 0));
        song.karaoke = true;
        // Starts 20 frames from the nearest keyframe: nowhere near a cut.
        let far = event("D", at(1, 20), at(2, 0));
        let (styles, _) = styles(&video_2s(), &[song, far]);
        assert_eq!(styles.len(), 1);
        assert_eq!(styles[0].counts, [0, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn a_style_is_timed_when_its_times_pile_up() {
        // 16 times or more: the top two offsets need 45%.
        assert!(is_timed(&[2, 2, 2, 5, 4, 3, 2])); // 20 times, 9 in the top two
        assert!(!is_timed(&[3, 2, 2, 4, 4, 3, 2])); // 20, 8
        // Shaped like E09: dialogue piled on +1, and an effects style flat at 279-314 each.
        assert!(is_timed(&[30, 24, 38, 29, 143, 31, 28]));
        assert!(!is_timed(&[279, 300, 314, 290, 285, 301, 296]));
        // Under 16: the top offset needs half.
        assert!(is_timed(&[0, 0, 0, 1, 1, 0, 0]));
        assert!(!is_timed(&[0, 0, 1, 1, 1, 0, 0]));
        // One time is no evidence.
        assert!(!is_timed(&[0, 0, 0, 1, 0, 0, 0]));
    }

    #[test]
    fn only_the_timed_styles_times_are_pooled_in_time_order() {
        // Dialogue starting a frame late, written last line first; its ends are far from cuts.
        let mut events: Vec<Event> = (0..10)
            .rev()
            .map(|n| event("D", at(n, 1), at(n, 30)))
            .collect();
        // Effects spread evenly over every offset.
        events.extend((0..21).map(|i| event("FX", at(100 + i, i % 7 - 3), at(100 + i, 30))));
        let (styles, pool) = styles(&video_2s(), &events);
        let timed: Vec<(&str, bool)> = styles.iter().map(|s| (s.style.as_str(), s.timed)).collect();
        assert_eq!(timed, [("D", true), ("FX", false)]);
        let want: Vec<Time> = (0..10)
            .map(|n| Time {
                ms: at(n, 1),
                offset: 1,
            })
            .collect();
        assert_eq!(pool, want);
    }

    /// A line every 6 s whose times sit `frames` frames after a keyframe, from `from_s` to
    /// `to_s`: 60 times in a 180 s window.
    fn dialogue(from_s: i64, to_s: i64, frames: i64) -> Vec<Event> {
        (from_s / 2..to_s / 2)
            .step_by(3)
            .map(|n| event("D", at(n, frames), at(n + 1, frames)))
            .collect()
    }

    /// A background in the same style, on keyframes [`dialogue`] does not use: a line every 12 s
    /// starting 3, 2 or 1 frame early or 2 or 3 late, and ending far from any cut. 15 times in a
    /// 180 s window.
    fn noise(from_s: i64, to_s: i64) -> Vec<Event> {
        (from_s / 2 + 2..to_s / 2)
            .step_by(6)
            .zip([-3, -2, -1, 2, 3].into_iter().cycle())
            .map(|(n, frames)| event("D", at(n, frames), at(n, 25)))
            .collect()
    }

    fn analysed(events: &[Event]) -> (Verdict, usize, usize, usize) {
        let a = analyse(&video_2s(), events);
        (a.verdict, a.times, a.agreeing, a.windows)
    }

    #[test]
    fn a_track_in_sync() {
        let events = [dialogue(0, 1440, 0), noise(0, 1440)].concat();
        assert_eq!(analysed(&events), (Verdict::InSync, 600, 8, 8));
    }

    #[test]
    fn a_track_one_frame_late() {
        let events = [dialogue(0, 1440, 1), noise(0, 1440)].concat();
        assert_eq!(analysed(&events), (Verdict::Shifted(1), 600, 8, 8));
    }

    #[test]
    fn a_track_two_frames_early() {
        let events = [dialogue(0, 1440, -2), noise(0, 1440)].concat();
        assert_eq!(analysed(&events), (Verdict::Shifted(-2), 600, 8, 8));
    }

    #[test]
    fn an_offset_needs_ten_times_and_twice_the_runner_up() {
        assert_eq!(decided(&[0, 0, 0, 5, 10, 0, 0]), Some(1));
        assert_eq!(decided(&[0, 0, 0, 6, 11, 0, 0]), None);
        assert_eq!(decided(&[0, 0, 0, 10, 0, 0, 0]), Some(0));
        assert_eq!(decided(&[0, 0, 0, 0, 9, 0, 0]), None);
    }

    #[test]
    fn a_short_tail_at_its_own_offset_decides_nothing() {
        // The last window holds only an ED's few lines, 4 times at -3 and 2 at +1, as at the end of
        // K-On!! E17. Over the background, that is 7 times at -3 against 3.
        let ed = [
            event("ED", at(640, -3), at(641, -3)),
            event("ED", at(650, -3), at(651, -3)),
            event("ED", at(660, 1), at(661, 1)),
        ];
        let events = [dialogue(0, 1260, 1), ed.to_vec(), noise(0, 1440)].concat();
        assert_eq!(analysed(&events), (Verdict::Shifted(1), 546, 7, 8));
    }

    #[test]
    fn an_undecided_window_does_not_split_a_stretch() {
        // 6:00 to 9:00 has only the background.
        let events = [dialogue(0, 360, 1), dialogue(540, 1440, 1), noise(0, 1440)].concat();
        assert_eq!(analysed(&events), (Verdict::Shifted(1), 540, 7, 8));
    }

    #[test]
    fn a_track_late_except_in_the_middle_is_partial_with_its_own_times_as_boundaries() {
        // +1 up to 7:44.04 (the end of the line at 7:42), 0 from 7:48 to 15:32, +1 from 15:42.04.
        // Between 15:32 and 15:42.04 one background line, 15:40.08 to 15:41, points nowhere: the
        // silence before it (8 s) is longer than the one after it (1 s), so it moves with the
        // late stretch.
        let events = [
            dialogue(0, 464, 1),
            dialogue(468, 932, 0),
            dialogue(942, 1440, 1),
            noise(0, 1440),
        ]
        .concat();
        let (verdict, _, agreeing, windows) = analysed(&events);
        assert_eq!(
            verdict,
            Verdict::Partial(vec![
                Stretch {
                    from_ms: None,
                    to_ms: Some(464_040),
                    frames: 1
                },
                Stretch {
                    from_ms: Some(468_000),
                    to_ms: Some(932_000),
                    frames: 0
                },
                Stretch {
                    from_ms: Some(940_080),
                    to_ms: None,
                    frames: 1
                },
            ])
        );
        assert_eq!((agreeing, windows), (0, 8));
    }

    #[test]
    fn karaoke_and_a_flat_effects_style_do_not_move_the_verdict() {
        // Sparse in-sync dialogue: a line every 12 s, 30 times at 0 in each window.
        let mut events: Vec<Event> = (0..720)
            .step_by(6)
            .map(|n| event("D", at(n, 0), at(n + 1, 0)))
            .collect();
        // Karaoke one frame late at every keyframe: counted, it would pile up at +1.
        events.extend((0..720).map(|n| Event {
            karaoke: true,
            ..event("OP", at(n, 1), at(n + 1, 1))
        }));
        // Effects at every keyframe starting on each offset and ending a frame late: 90 times at
        // each offset and 180 at +1 per window. Counted, they would outvote the dialogue's 0.
        for n in 0..720 {
            events.extend((-3..=3).map(|frames| event("FX", at(n, frames), at(n, 1))));
        }
        let a = analyse(&video_2s(), &events);
        assert_eq!((a.verdict, a.times), (Verdict::InSync, 240));
        let fx = a.styles.iter().find(|s| s.style == "FX").unwrap();
        assert_eq!(
            (fx.counts, fx.timed),
            ([720, 720, 720, 720, 1440, 720, 720], false)
        );
    }

    #[test]
    fn with_no_window_decided_ten_times_decide_and_nine_are_too_few() {
        // One line per window, starting a frame late and ending far from any cut.
        let line = |w: i64| event("D", at(w * 90 + 1, 1), at(w * 90 + 1, 25));
        let nine: Vec<Event> = (0..9).map(line).collect();
        assert_eq!(analysed(&nine), (Verdict::TooFew, 9, 0, 9));
        let ten: Vec<Event> = (0..10).map(line).collect();
        assert_eq!(analysed(&ten), (Verdict::Shifted(1), 10, 0, 10));
    }

    #[test]
    fn many_times_with_no_offset_standing_out_are_unclear() {
        // E09's Signs track: 26 times spread 6/4/2/6/7/1/0 over -3 to +3, about 3 per window.
        let offsets = [
            [-3; 6].as_slice(),
            &[-2; 4],
            &[-1; 2],
            &[0; 6],
            &[1; 7],
            &[2; 1],
        ]
        .concat();
        let events: Vec<Event> = (0..26)
            .zip(offsets)
            .map(|(i, frames)| {
                let n = (i % 8) * 90 + i / 8 * 3 + 1;
                event("S", at(n, frames), at(n, 25))
            })
            .collect();
        assert_eq!(analysed(&events), (Verdict::Unclear, 26, 0, 8));
    }

    fn report(
        id: usize,
        name: &str,
        verdict: Verdict,
        times: usize,
        agreeing: usize,
    ) -> TrackReport {
        TrackReport {
            id,
            name: name.into(),
            result: Ok(Analysis {
                verdict,
                times,
                pool: [0; OFFSETS],
                windows: 9,
                agreeing,
                styles: Vec::new(),
                frame_ns: FRAME_NS,
                each: Vec::new(),
                unshifted: Vec::new(),
            }),
            lines: None,
        }
    }

    #[test]
    fn a_whole_track_shift_prints_its_fix() {
        let tracks = [
            report(5, "English Full [Semisu]", Verdict::Shifted(1), 289, 7),
            report(6, "English Signs/Songs [Semisu]", Verdict::Unclear, 26, 0),
        ];
        assert_eq!(
            render("K-On!! - E09 - Finals.mkv", &tracks, false),
            "K-On!! - E09 - Finals.mkv
  track 5  English Full [Semisu]         1 frame late     (289 times, +1 in 7 of 9 windows)
           fix: mkvmerge --sync 5:-42
  track 6  English Signs/Songs [Semisu]  unclear          (26 times, no offset stands out)
"
        );
    }

    #[test]
    fn a_partial_track_prints_each_stretch_by_time() {
        let stretches = vec![
            Stretch {
                from_ms: None,
                to_ms: Some(475_780),
                frames: 1,
            },
            Stretch {
                from_ms: Some(483_510),
                to_ms: Some(939_000),
                frames: 0,
            },
            Stretch {
                from_ms: Some(940_470),
                to_ms: None,
                frames: 1,
            },
        ];
        let mut track = report(
            5,
            "English Full [Semisu]",
            Verdict::Partial(stretches),
            250,
            0,
        );
        // E16 has none; one here for the layout.
        if let Ok(a) = &mut track.result {
            a.unshifted = vec![(1_200_000, 1_203_500)];
        }
        assert_eq!(
            render("K-On!! - E16 - Upperclassmen.mkv", &[track], false),
            "K-On!! - E16 - Upperclassmen.mkv
  track 5  English Full [Semisu]  partial
           1 frame late  start      – 0:07:55.78
           in sync       0:08:03.51 – 0:15:39.00
           1 frame late  0:15:40.47 – end
           not shifted   0:20:00.00 – 0:20:03.50
"
        );
    }

    #[test]
    fn in_sync_too_few_unsupported_and_a_verdict_from_the_pool() {
        let mut pooled = report(4, "Signs", Verdict::Shifted(-2), 40, 0);
        if let Ok(a) = &mut pooled.result {
            a.pool[slot(-2)] = 22;
        }
        let tracks = [
            report(2, "English Full [Tsundere]", Verdict::InSync, 363, 8),
            report(3, "Songs", Verdict::TooFew, 9, 0),
            pooled,
            TrackReport {
                id: 12,
                name: "(jpn)".into(),
                result: Err("unsupported: no video track".into()),
                lines: None,
            },
        ];
        assert_eq!(
            render("a.mkv", &tracks, false),
            "a.mkv
  track 2   English Full [Tsundere]  in sync          (363 times, 0 in 8 of 9 windows)
  track 3   Songs                    too few lines    (9 times)
  track 4   Signs                    2 frames early   (40 times, 22 of them at -2)
            fix: mkvmerge --sync 4:83
  track 12  (jpn)                    unsupported: no video track
"
        );
    }

    #[test]
    fn verbose_adds_each_styles_counts() {
        let mut r = report(0, "Full", Verdict::InSync, 300, 8);
        if let Ok(a) = &mut r.result {
            a.styles = vec![
                StyleCounts {
                    style: "Default".into(),
                    counts: [24, 30, 38, 143, 29, 31, 28],
                    timed: true,
                },
                StyleCounts {
                    style: "OP ro".into(),
                    counts: [279, 300, 314, 290, 285, 301, 296],
                    timed: false,
                },
            ];
        }
        assert_eq!(
            render("a.mkv", &[r], true),
            "a.mkv
  track 0  Full  in sync          (300 times, 0 in 8 of 9 windows)
           style     -3   -2   -1    0   +1   +2   +3
           Default   24   30   38  143   29   31   28  used
           OP ro    279  300  314  290  285  301  296  ignored
"
        );
    }

    #[test]
    fn a_track_without_a_verdict_prints_the_one_its_shared_lines_give() {
        let follow = shared::Follow {
            source: 5,
            gap: 0,
            shared: shared::Shared {
                times: 11_268,
                counts: [0, 0, 0, 11_268, 0, 0, 0],
            },
            verdict: Verdict::Shifted(1),
            each: Vec::new(),
            unshifted: Vec::new(),
        };
        let mut signs = report(6, "English Signs/Songs [Semisu]", Verdict::Unclear, 23, 0);
        signs.lines = Some(shared::Lines::Follows(follow));
        let mut sdh = report(7, "SDH", Verdict::InSync, 300, 8);
        sdh.lines = Some(shared::Lines::Disagrees {
            source: 5,
            says: Verdict::Shifted(1),
        });
        let tracks = [
            report(5, "English Full [Semisu]", Verdict::Shifted(1), 345, 5),
            signs,
            sdh,
        ];
        assert_eq!(
            render("K-On!! - E09 - Finals.mkv", &tracks, false),
            "K-On!! - E09 - Finals.mkv
  track 5  English Full [Semisu]         1 frame late     (345 times, +1 in 5 of 9 windows)
           fix: mkvmerge --sync 5:-42
  track 6  English Signs/Songs [Semisu]  unclear          (23 times, no offset stands out)
           timed as track 5 (11268 of 11268 shared times): 1 frame late
           fix: mkvmerge --sync 6:-42
  track 7  SDH                           in sync          (300 times, 0 in 8 of 9 windows)
           but its lines shared with track 5 say 1 frame late
"
        );
    }

    #[test]
    fn a_file_with_nothing_to_check_says_so() {
        assert_eq!(
            render("a.mkv", &[], false),
            "a.mkv\n  no text subtitle tracks to check\n"
        );
    }

    fn track(number: u64, kind: u64, default_duration: Option<u64>) -> Track {
        Track {
            id: usize::try_from(number - 1).unwrap(),
            number,
            uid: number,
            kind,
            codec: String::new(),
            name: None,
            language: "eng".into(),
            language_element: true,
            language_bcp47: None,
            default: true,
            forced: false,
            default_duration,
            codec_private: Vec::new(),
            encodings: Vec::new(),
            flags: crate::mkv::Flags::default(),
            video: None,
            audio: None,
        }
    }

    fn layout(tracks: Vec<Track>, cue_times: &[u64]) -> Layout {
        Layout {
            segment_data: 0,
            segment_end: 0,
            timestamp_scale: 1_000_000,
            duration_ns: None,
            tracks,
            cues: cue_times
                .iter()
                .map(|&time| Cue {
                    time,
                    track: 1,
                    cluster: 0,
                    relative: None,
                })
                .collect(),
            frame_counts: std::collections::HashMap::new(),
            byte_counts: std::collections::HashMap::new(),
            segment_uid: None,
            title: None,
            chapters_at: None,
            attachments_at: None,
            tags_at: None,
            first_cluster: None,
            truncated: None,
        }
    }

    fn every_5_s(n: u64) -> Vec<u64> {
        (0..n).map(|i| i * 5000).collect()
    }

    #[test]
    fn the_video_is_its_frame_duration_and_its_keyframe_cues() {
        // Out of order, one time twice, and a subtitle Cue that is not a keyframe.
        let mut times = every_5_s(10);
        times.reverse();
        times.push(5000);
        let mut l = layout(
            vec![
                track(1, TRACK_TYPE_VIDEO, Some(FRAME_NS)),
                track(2, TRACK_TYPE_SUBTITLE, None),
            ],
            &times,
        );
        l.cues.push(Cue {
            time: 1234,
            track: 2,
            cluster: 0,
            relative: None,
        });
        let v = video(&l).unwrap();
        assert_eq!(v.frame_ns, FRAME_NS);
        assert_eq!(
            v.keyframes_ns,
            every_5_s(10)
                .iter()
                .map(|t| t * 1_000_000)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_video_that_cannot_stand_in_for_cuts_says_why() {
        let subs = layout(vec![track(1, TRACK_TYPE_SUBTITLE, None)], &every_5_s(10));
        assert_eq!(video(&subs).unwrap_err(), "no video track");

        let vfr = layout(vec![track(1, TRACK_TYPE_VIDEO, None)], &every_5_s(10));
        assert_eq!(
            video(&vfr).unwrap_err(),
            "no DefaultDuration on the video track"
        );

        let few = layout(
            vec![track(1, TRACK_TYPE_VIDEO, Some(FRAME_NS))],
            &every_5_s(9),
        );
        assert_eq!(
            video(&few).unwrap_err(),
            "only 9 video keyframes in the Cues"
        );

        // 10 keyframes over 200 s: one per 20 s.
        let sparse_times: Vec<u64> = (0..10).map(|i| i * 200_000 / 9).collect();
        let sparse = layout(
            vec![track(1, TRACK_TYPE_VIDEO, Some(FRAME_NS))],
            &sparse_times,
        );
        assert_eq!(
            video(&sparse).unwrap_err(),
            "10 video keyframes in the Cues over 0:03:20.00: fewer than one per 10 s"
        );
    }

    #[test]
    fn a_keyframe_cue_on_every_frame_or_field_is_not_a_cut() {
        // 25 fps, 100 Cues: on every frame (all-intra video, or a mux with `--cues 0:all`), on every
        // other (the same with DefaultDuration a field), or on every fourth.
        let video_track = |frame_ns| vec![track(1, TRACK_TYPE_VIDEO, Some(frame_ns))];
        let every = |frames: u64| -> Vec<u64> { (0..100).map(|i| i * 40 * frames).collect() };
        assert_eq!(
            video(&layout(video_track(40_000_000), &every(1))).unwrap_err(),
            "100 video keyframes in the Cues over 0:00:03.96: more than one per 4 frames"
        );
        assert!(video(&layout(video_track(40_000_000), &every(2))).is_err());
        assert!(video(&layout(video_track(40_000_000), &every(4))).is_ok());
        // A corrupt DefaultDuration is unsupported, not a crash.
        assert!(video(&layout(video_track(u64::MAX), &every(4))).is_err());
    }

    #[test]
    fn a_time_the_smallest_i64_frames_from_its_keyframe_is_not_counted() {
        // At 1 ns a frame, 0 ms is i64::MIN frames before keyframes this far in.
        let first = 9_223_372_036_854_275_808;
        let video = Video {
            frame_ns: 1,
            keyframes_ns: (0..10).map(|i| first + i * 1_000).collect(),
        };
        assert_eq!(frame_offset(&video, 0), i128::from(i64::MIN));
        // Neither time is near a cut: no style has a time to count.
        let (styles, pool) = styles(&video, &[event("D", 0, 1)]);
        assert!(styles.is_empty() && pool.is_empty());
    }

    #[test]
    fn a_time_more_frames_from_its_keyframe_than_an_i64_counts_is_as_many_frames_off() {
        // At 1 ns a frame, 0 ms is past i64::MIN frames before keyframes this far in, and the end
        // of i64 ms past i64::MAX frames after the last.
        let first = 10_000_000_000_000_000_000;
        let video = Video {
            frame_ns: 1,
            keyframes_ns: (0..10).map(|i| first + i * 1_000).collect(),
        };
        assert_eq!(frame_offset(&video, 0), -10_000_000_000_000_500_000);
        assert_eq!(
            frame_offset(&video, i64::MAX),
            9_223_362_036_854_775_806_491_000
        );
    }
}
