//! `subcheck audio`: whether the audio tracks that hold the same sound line up.
//!
//! Every audio track is decoded (by ffmpeg, see [`decode`]) in a few windows spread over the file,
//! and every pair of tracks is cross-correlated in each window. Two tracks whose sound matches in
//! (nearly) every window hold the same audio, in whatever format; each is reported against the
//! lower-numbered track it matches best: in sync, late or early by a steady amount, or drifting.
//! Pure functions over decoded windows; `main` runs ffmpeg.

pub mod correlate;
pub mod decode;

use std::fmt::Write;

use correlate::Lag;

/// Each window is this long.
pub const WINDOW_NS: u64 = 10_000_000_000;

/// A window where both tracks have sound matches when it scores at least this; one that scores
/// less is unclear and says nothing of the lag. Lossy copies match in every window: the AAC copies
/// of K-On's FLAC score 0.26 at worst (most windows 0.4-0.9), FMAB's AC-3 against its TrueHD 0.94.
/// A dub shares the music and effects, so its OP and ED windows score up to 0.97, but in 10 of the
/// 11 episodes measured (FMAB, K-On) at least two of six windows of dialogue scored 0.23 or less.
/// Commentaries score below it everywhere.
pub const MATCH_SCORE: f64 = 0.25;

/// Tracks tagged with different languages hold the same sound only when every window with sound
/// in both scores at least this, as a copy in another codec does: the check is for one audio in
/// two codecs, not a dub against its original. A dub shares the original's music and effects, so
/// an episode of action can match in every window: FMAB's dubs score 0.37 at worst in their worst
/// window (E61), its AC-3 copies 0.88 or more. A copy tagged with the wrong language is caught so
/// when it is clean; a poor one (K-On's AAC copies of its FLAC, 0.26-0.49) is not.
pub const COPY_SCORE: f64 = 0.5;

/// How much less alike (median score) than the best match a track may be and still be the one
/// reported against. Two copies of one sound match a third about as well as each other; a dub
/// that matches through its music and effects (FMAB E61's AC-3 against the Japanese, 0.63 where
/// both are tagged alike) does not come close to its own track (0.97).
const ALIKE_SLACK: f64 = 0.05;

/// How many of a pair's windows with sound may be unclear with the pair still the same audio:
/// one in four. A sync step inside a window splits its sound between two lags, and its score with
/// it. Not one in three: a dub, whose dialogue windows miss, would pass then.
fn misses_allowed(heard: usize) -> usize {
    heard / 4
}

/// A pair needs sound in this many windows, or in all of them in a file too short for this many.
pub const MIN_WINDOWS: usize = 3;

/// A steady lag this small (ms either way) is in sync. Matroska stores times to the millisecond,
/// and a window starts where each track's first frame there says, so one window may read up to
/// a millisecond off for a pair in sync.
pub const IN_SYNC_MS: f64 = 1.0;

/// Clear windows whose lags spread wider than this (ms) drift. Each may carry the rounding above,
/// and a lossy copy's lag wanders with its codec: K-On!!'s AAC copies, steadily 21 ms late, spread
/// 0.7-2.2 ms over a file. Its steps of 8 ms (S00E05, S00E08) and FMAB's 67 ms head hole drift.
pub const STEADY_MS: f64 = 3.0;

/// Where to decode a file of `duration_ns`: ten-second windows from 0:30, and at 20, 40, 60 and
/// 80 % in, and a minute before the end. Spreading them catches a lag that changes over the file,
/// like the hole FMAB's TrueHD had at its head, 67 ms early then and in sync by the end.
pub fn windows(duration_ns: u64) -> Vec<(u64, u64)> {
    spread(duration_ns, WINDOW_NS, 30_000_000_000)
}

/// `len_ns` at a time over a file of `duration_ns`, as (start, length) in ns, in time order: from
/// `first_ns`, at 20, 40, 60 and 80 % in, and a minute before the end. One past the end or over
/// an earlier one is dropped; with none left, the file's start (the whole of a file shorter than
/// `len_ns`) is the one. apply's check reads its 5-second spans at these points too, from the
/// head instead of 0:30.
pub fn spread(duration_ns: u64, len_ns: u64, first_ns: u64) -> Vec<(u64, u64)> {
    let mut starts: Vec<u64> = [20, 40, 60, 80]
        .iter()
        .map(|&percent| duration_ns / 100 * percent)
        .chain([first_ns])
        .chain(duration_ns.checked_sub(60_000_000_000))
        .filter(|&s| s + len_ns <= duration_ns)
        .collect();
    starts.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for s in starts {
        if out.last().is_none_or(|&(prev, len)| s >= prev + len) {
            out.push((s, len_ns));
        }
    }
    if out.is_empty() && duration_ns > 0 {
        out.push((0, duration_ns.min(len_ns)));
    }
    out
}

/// What one pair's windows say.
#[derive(Debug, Clone, PartialEq)]
pub enum Match {
    /// Too few windows with sound in both tracks to tell.
    TooQuiet,
    /// The tracks hold different sound.
    Different,
    /// The same audio, lined up as the clear windows say.
    Same(Lineup),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lineup {
    InSync,
    /// Steadily off by `ms` (the clear windows' median): positive when the second track is late.
    Off {
        ms: f64,
    },
    /// The clear windows disagree by more than [`STEADY_MS`].
    Drifts,
}

/// A pair's verdict from its windows' lags (`None` where either track is quiet).
pub fn assess(lags: &[Option<Lag>]) -> Match {
    let heard: Vec<&Lag> = lags.iter().flatten().collect();
    if heard.is_empty() || heard.len() < MIN_WINDOWS.min(lags.len()) {
        return Match::TooQuiet;
    }
    let misses = heard.iter().filter(|l| l.score < MATCH_SCORE).count();
    if misses > misses_allowed(heard.len()) {
        return Match::Different;
    }
    let clear: Vec<f64> = heard
        .iter()
        .filter(|l| l.score >= MATCH_SCORE)
        .map(|l| l.ms)
        .collect();
    let (lo, hi) = clear
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &ms| {
            (lo.min(ms), hi.max(ms))
        });
    let ms = median(clear);
    Match::Same(if hi - lo > STEADY_MS {
        Lineup::Drifts
    } else if ms.abs() <= IN_SYNC_MS {
        Lineup::InSync
    } else {
        Lineup::Off { ms }
    })
}

/// How alike a pair's sound is: the median score of the windows where both have sound.
fn alike(lags: &[Option<Lag>]) -> f64 {
    let scores: Vec<f64> = lags.iter().flatten().map(|l| l.score).collect();
    if scores.is_empty() {
        0.0
    } else {
        median(scores)
    }
}

/// The median of a non-empty list.
fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        v[mid]
    } else {
        f64::midpoint(v[mid - 1], v[mid])
    }
}

/// An audio track, as the report names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: usize,
    pub name: String,
    /// ISO 639-2, as tagged (or Matroska's default `eng`); `None` for a track tagged only in BCP 47.
    pub language: Option<String>,
    /// LanguageBCP47, as tagged.
    pub language_bcp47: Option<String>,
}

impl Track {
    /// The report's view of a file's audio track, its language tags as Matroska reads them: a
    /// track with no Language element is `eng`, unless it has a LanguageBCP47 (which Language
    /// would only stand beside).
    pub fn of(t: &crate::mkv::Track) -> Track {
        let bcp47 = t.language_bcp47.clone();
        let language = (t.language_element || bcp47.is_none()).then(|| t.language.clone());
        Track {
            id: t.id,
            name: t.shown_name(),
            language,
            language_bcp47: bcp47,
        }
    }
}

/// ISO 639-2's bibliographic codes, each with its terminology twin: one language, two codes.
const BIBLIOGRAPHIC: [(&str, &str); 20] = [
    ("alb", "sqi"),
    ("arm", "hye"),
    ("baq", "eus"),
    ("bur", "mya"),
    ("chi", "zho"),
    ("cze", "ces"),
    ("dut", "nld"),
    ("fre", "fra"),
    ("geo", "kat"),
    ("ger", "deu"),
    ("gre", "ell"),
    ("ice", "isl"),
    ("mac", "mkd"),
    ("mao", "mri"),
    ("may", "msa"),
    ("per", "fas"),
    ("rum", "ron"),
    ("slo", "slk"),
    ("tib", "bod"),
    ("wel", "cym"),
];

/// A track's BCP 47 language, as compared (lowercase: its primary subtag with an extended one,
/// `zh-yue`, but not its script or region, so `en-us` is `en-gb`) and as tagged. `None` without a
/// LanguageBCP47.
fn bcp47(t: &Track) -> Option<(String, String)> {
    let tag = t.language_bcp47.as_deref()?;
    let lower = tag.to_lowercase();
    let mut subtags = lower.split('-');
    let primary = subtags.next().unwrap_or_default();
    let compared = match subtags.next() {
        Some(ext) if ext.len() == 3 && ext.chars().all(|c| c.is_ascii_alphabetic()) => {
            format!("{primary}-{ext}")
        }
        _ => primary.to_string(),
    };
    Some((compared, tag.to_string()))
}

/// A track's ISO 639-2 language, as compared (lowercase, a bibliographic code as its terminology
/// twin: `fre` is `fra`) and as tagged. `None` for a track tagged only in BCP 47.
fn iso639_2(t: &Track) -> Option<(String, String)> {
    let tag = t.language.as_deref()?;
    let code = tag.to_lowercase();
    let compared = BIBLIOGRAPHIC
        .iter()
        .find(|(b, _)| *b == code)
        .map_or(code.clone(), |(_, t)| (*t).to_string());
    Some((compared, tag.to_string()))
}

/// Two tracks' languages, each as compared and as tagged, in one kind of tag: BCP 47 when both
/// have one (two dubs tagged `chi` differ as `zh-yue` and `zh-Hans`), else ISO 639-2 when both
/// have it. Never one kind against the other, which would take a table of every language: a track
/// tagged only in BCP 47 and one only in ISO 639-2 differ, as nothing says they are the same. `und`
/// is a language like any other.
fn languages(a: &Track, b: &Track) -> ((String, String), (String, String)) {
    if let (Some(x), Some(y)) = (bcp47(a), bcp47(b)) {
        return (x, y);
    }
    if let (Some(x), Some(y)) = (iso639_2(a), iso639_2(b)) {
        return (x, y);
    }
    // At most one tag kind each, not the same one: each by its own kind, marked so they differ.
    let own = |t: &Track| match (bcp47(t), iso639_2(t)) {
        (Some((c, shown)), _) => (format!("bcp47 {c}"), shown),
        (None, Some((c, shown))) => (format!("iso639-2 {c}"), shown),
        (None, None) => ("none".to_string(), "und".to_string()),
    };
    (own(a), own(b))
}

/// Two tracks' languages as tagged, when they differ as compared.
fn shown_languages(a: &Track, b: &Track) -> Option<(String, String)> {
    let ((x, mine), (y, theirs)) = languages(a, b);
    (x != y).then_some((mine, theirs))
}

/// Two tracks' lags, window by window, the second's against the first's; `a` < `b`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    pub a: usize,
    pub b: usize,
    pub lags: Vec<Option<Lag>>,
}

/// What the report says of one track.
#[derive(Debug, Clone, PartialEq)]
pub enum Said {
    /// Others are reported against it.
    Reference,
    /// The same audio as the lower-numbered track `reference`.
    Against {
        reference: usize,
        lineup: Lineup,
        advice: Advice,
        /// This track's language and the reference's, when they differ (see `shown_languages`).
        languages: Option<(String, String)>,
    },
    /// Track `by` holds its sound too, but is reported against another track it matches better.
    Matched { by: usize },
    /// No other track holds its sound.
    Alone,
    /// No match, and too little sound to tell for some pair.
    TooQuiet,
}

/// What to do about a track reported against another. Above it means up its references, past
/// those in sync with theirs, to the first that is off (or to the top of its group, or to a track
/// that drifts: no `--sync` moves a drift, so what goes with one lines up with it as with a top).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advice {
    /// Nothing: in sync, with nothing above it off; or a drift, whose own line says no `--sync`
    /// fixes it.
    Nothing,
    /// `mkvmerge --sync <track>:<ms>` lines it up, nothing above it being off. `or_reference`:
    /// moving the reference by -ms would do instead, as it is the top of the group, no other track
    /// holds its sound, and none is reported against this one.
    Fix { ms: i64, or_reference: bool },
    /// In sync, and above it is track `of`, whose fix moves it by `ms`: the same move.
    With { ms: i64, of: usize },
    /// Above it is track `of`, off: no fix until that one is fixed.
    After { of: usize },
}

/// Each track's line, in track order: every track is reported against the lower-numbered track
/// whose sound it matches best, so a compatibility AC-3 goes with its TrueHD even where a dub
/// before them both matches it through the music. Of tracks that match it about as well (within
/// [`ALIKE_SLACK`]), the lowest-numbered: two copies of one sound are as good as each other.
/// Tracks tagged with different languages match only as a copy does ([`COPY_SCORE`]).
pub fn report(tracks: &[Track], pairs: &[Pair]) -> Vec<(usize, Said)> {
    // Two languages that differ: a dub, unless it matches as a copy.
    let differ = |a: usize, b: usize| {
        let track = |id: usize| tracks.iter().find(|t| t.id == id);
        track(a)
            .zip(track(b))
            .is_some_and(|(a, b)| shown_languages(a, b).is_some())
    };
    let verdicts: Vec<Match> = pairs
        .iter()
        .map(|p| {
            let verdict = assess(&p.lags);
            if matches!(verdict, Match::Same(_))
                && differ(p.a, p.b)
                && !p.lags.iter().flatten().all(|l| l.score >= COPY_SCORE)
            {
                Match::Different
            } else {
                verdict
            }
        })
        .collect();
    let pair = |a: usize, b: usize| {
        let (a, b) = (a.min(b), a.max(b));
        pairs.iter().position(|p| p.a == a && p.b == b)
    };
    let reference_of = |t: &Track| {
        let matches: Vec<(&Track, Lineup, f64)> = tracks
            .iter()
            .filter(|r| r.id < t.id)
            .filter_map(|r| {
                let i = pair(r.id, t.id)?;
                match verdicts[i] {
                    Match::Same(lineup) => Some((r, lineup, alike(&pairs[i].lags))),
                    _ => None,
                }
            })
            .collect();
        let best = matches
            .iter()
            .map(|m| m.2)
            .fold(f64::NEG_INFINITY, f64::max);
        matches
            .into_iter()
            .find(|m| m.2 >= best - ALIKE_SLACK)
            .map(|(r, lineup, _)| (r, lineup))
    };
    let against: Vec<Option<(&Track, Lineup)>> = tracks.iter().map(reference_of).collect();
    let same =
        |a: usize, b: usize| pair(a, b).is_some_and(|i| matches!(verdicts[i], Match::Same(_)));
    let advice = advise(tracks, &against, same);
    tracks
        .iter()
        .zip(&against)
        .zip(&advice)
        .map(|((t, mine), advice)| {
            let said = if let Some((r, lineup)) = mine {
                Said::Against {
                    reference: r.id,
                    lineup: *lineup,
                    advice: *advice,
                    languages: shown_languages(t, r),
                }
            } else if against.iter().flatten().any(|(r, _)| r.id == t.id) {
                Said::Reference
            } else if let Some(by) = tracks
                .iter()
                .find(|x| x.id != t.id && same(t.id, x.id))
                .map(|x| x.id)
            {
                Said::Matched { by }
            } else if pairs
                .iter()
                .zip(&verdicts)
                .any(|(p, m)| (p.a == t.id || p.b == t.id) && *m == Match::TooQuiet)
            {
                Said::TooQuiet
            } else {
                Said::Alone
            };
            (t.id, said)
        })
        .collect()
}

/// Each reported track's advice (`Nothing` for the rest), given `against`, each track's reference
/// and lineup, in track order, and `same(a, b)`, whether tracks `a` and `b` hold the same sound.
fn advise(
    tracks: &[Track],
    against: &[Option<(&Track, Lineup)>],
    same: impl Fn(usize, usize) -> bool,
) -> Vec<Advice> {
    let reference = |id: usize| {
        tracks
            .iter()
            .position(|t| t.id == id)
            .and_then(|i| against[i])
    };
    // From `from` up its references, past those in sync with theirs (every reference is
    // lower-numbered, so the walk ends): the first that is off. A track that drifts ends the walk
    // as the top does: no `--sync` moves it, so what goes with it is fixed against it.
    let above = |from: usize| {
        let mut at = from;
        loop {
            match reference(at) {
                None | Some((_, Lineup::Drifts)) => return None,
                Some((r, Lineup::InSync)) => at = r.id,
                Some((_, _)) => return Some(at),
            }
        }
    };
    #[allow(clippy::cast_possible_truncation)] // a second at most
    let undo = |ms: f64| -(ms.round() as i64);
    // Track `u`'s own fix, when nothing above it is off.
    let fix_of = |u: usize| match reference(u)? {
        (r, Lineup::Off { ms }) if above(r.id).is_none() => Some(undo(ms)),
        _ => None,
    };
    tracks
        .iter()
        .zip(against)
        .map(|(t, mine)| {
            let Some((r, lineup)) = *mine else {
                return Advice::Nothing;
            };
            match (lineup, above(r.id)) {
                (Lineup::Drifts, _) | (Lineup::InSync, None) => Advice::Nothing,
                (Lineup::InSync, Some(of)) => match fix_of(of) {
                    Some(ms) => Advice::With { ms, of },
                    None => Advice::After { of },
                },
                (Lineup::Off { .. }, Some(of)) => Advice::After { of },
                (Lineup::Off { ms }, None) => {
                    let top = reference(r.id).is_none();
                    let alone = tracks
                        .iter()
                        .all(|x| x.id == t.id || x.id == r.id || !same(r.id, x.id));
                    let followed = against.iter().flatten().any(|(x, _)| x.id == t.id);
                    Advice::Fix {
                        ms: undo(ms),
                        or_reference: top && alone && !followed,
                    }
                }
            }
        })
        .collect()
}

/// A file's block: a line per audio track, with what to do about any that is off. `windows` are
/// the starts the pairs' lags were measured at. `verbose` adds every pair's lag and score in every
/// window.
pub fn render(
    file: &str,
    tracks: &[Track],
    windows: &[u64],
    pairs: &[Pair],
    verbose: bool,
) -> String {
    let mut out = format!("{file}\n");
    match tracks.len() {
        0 => {
            out.push_str("  no audio tracks\n");
            return out;
        }
        1 => {
            out.push_str("  one audio track, nothing to compare\n");
            return out;
        }
        _ => {}
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
    let said = report(tracks, pairs);
    for (t, (_, said)) in tracks.iter().zip(&said) {
        let line = format!(
            "  track {:<id_width$}  {:<name_width$}  {}",
            t.id,
            t.name,
            verdict(said)
        );
        out.push_str(line.trim_end());
        out.push('\n');
        if let Said::Against { reference, .. } = said {
            let lags = &pairs
                .iter()
                .find(|p| p.a == *reference && p.b == t.id)
                .expect("a track is reported against a pair it is in")
                .lags;
            details(&mut out, &indent, t, said, windows, lags);
        }
    }
    if verbose {
        for p in pairs {
            let each: Vec<String> = windows
                .iter()
                .zip(&p.lags)
                .map(|(&at, lag)| match lag {
                    None => format!("{} quiet", clock(at)),
                    Some(l) => format!("{} {} ms {:.2}", clock(at), signed(l.ms, 2), l.score),
                })
                .collect();
            let _ = writeln!(out, "  tracks {} and {}: {}", p.a, p.b, each.join("  "));
        }
    }
    out
}

/// What a track's line says of it.
fn verdict(said: &Said) -> String {
    match said {
        Said::Reference => "reference".to_string(),
        Said::Alone => "no other track holds this audio".to_string(),
        Said::Matched { by } => {
            format!("track {by} holds this audio too, but goes with another track")
        }
        Said::TooQuiet => "too little sound to compare".to_string(),
        Said::Against {
            reference, lineup, ..
        } => match lineup {
            Lineup::InSync => format!("in sync with track {reference}"),
            Lineup::Off { ms } => format!(
                "{:.1} ms {} against track {reference}",
                ms.abs(),
                if *ms > 0.0 { "late" } else { "early" }
            ),
            Lineup::Drifts => format!("drifts against track {reference}"),
        },
    }
}

/// The lines under a track reported against another: its advice; each window's lag for a drift;
/// the windows that said nothing; and the other track's language where it differs.
fn details(
    out: &mut String,
    indent: &str,
    t: &Track,
    said: &Said,
    windows: &[u64],
    lags: &[Option<Lag>],
) {
    let Said::Against {
        reference,
        lineup,
        advice,
        languages,
    } = said
    else {
        return;
    };
    let id = t.id;
    let _ = match *advice {
        Advice::Nothing => Ok(()),
        Advice::Fix {
            ms,
            or_reference: false,
        } => writeln!(out, "{indent}fix: mkvmerge --sync {id}:{ms}"),
        Advice::Fix {
            ms,
            or_reference: true,
        } => writeln!(
            out,
            "{indent}fix: mkvmerge --sync {id}:{ms}, or --sync {reference}:{} if track {reference} is the one off",
            -ms
        ),
        Advice::With { ms, of } => writeln!(
            out,
            "{indent}fix: mkvmerge --sync {id}:{ms}, the same move as track {of}'s"
        ),
        Advice::After { of } => writeln!(
            out,
            "{indent}no fix until track {of} is fixed: check again then"
        ),
    };
    match lineup {
        Lineup::Drifts => {
            let each: Vec<String> = windows
                .iter()
                .zip(lags)
                .map(|(&at, lag)| {
                    let v = match lag {
                        None => "quiet".to_string(),
                        Some(l) if l.score < MATCH_SCORE => "?".to_string(),
                        Some(l) => signed(l.ms, 1),
                    };
                    format!("{} {v}", clock(at))
                })
                .collect();
            let _ = writeln!(
                out,
                "{indent}no one --sync fixes a drift; ms late by window: {}",
                each.join("  ")
            );
        }
        Lineup::InSync | Lineup::Off { .. } => {}
    }
    if !matches!(lineup, Lineup::Drifts) {
        let unclear: Vec<String> = windows
            .iter()
            .zip(lags)
            .filter_map(|(&at, lag)| {
                lag.filter(|l| l.score < MATCH_SCORE)
                    .map(|l| format!("{} (score {:.2})", clock(at), l.score))
            })
            .collect();
        if !unclear.is_empty() {
            let _ = writeln!(out, "{indent}unclear at {}", unclear.join(", "));
        }
    }
    if let Some((mine, theirs)) = languages {
        let _ = writeln!(out, "{indent}tagged {mine}, track {reference} {theirs}");
    }
}

/// `ms` to `places` decimals with its sign, and no `-0.0` for a lag a hair below none.
fn signed(ms: f64, places: usize) -> String {
    let s = format!("{ms:+.places$}");
    if s.trim_start_matches(['-', '+'])
        .chars()
        .all(|c| c == '0' || c == '.')
    {
        s.replacen('-', "+", 1)
    } else {
        s
    }
}

/// `m:ss`, or `h:mm:ss` from an hour.
fn clock(ns: u64) -> String {
    let s = ns / 1_000_000_000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    #[allow(clippy::unnecessary_wraps)] // a window's lag as a pair holds it: None is quiet
    fn lag(ms: f64, score: f64) -> Option<Lag> {
        Some(Lag { ms, score })
    }

    #[test]
    fn windows_spread_over_the_file_and_never_overlap() {
        let starts = |d: u64| windows(d * S).iter().map(|w| w.0 / S).collect::<Vec<_>>();
        // 24 min: 0:30, 20-80 %, a minute before the end.
        assert_eq!(starts(1440), [30, 288, 576, 864, 1152, 1380]);
        // 50 s: 0:30 lands on 60 %; nothing a minute before the end.
        assert_eq!(starts(50), [10, 20, 30, 40]);
        // 90 s: 20 % is 18 s, 0:30 overlaps 40 % (36 s), so 36 s goes.
        assert_eq!(starts(90), [18, 30, 54, 72]);
        // Too short for any: the first ten seconds, or the whole file.
        assert_eq!(windows(12 * S), [(0, 10 * S)]);
        assert_eq!(windows(4 * S), [(0, 4 * S)]);
        assert_eq!(windows(0), []);
    }

    #[test]
    fn two_tracks_hold_the_same_audio_when_nearly_every_window_matches() {
        // K-On! E01's dub: its OP and ED windows match through the music, the dialogue does not.
        let dub = [
            lag(-0.5, 0.29),
            lag(912.0, 0.03),
            lag(-0.6, 0.17),
            lag(-0.7, 0.07),
            lag(-997.0, 0.05),
            lag(-0.3, 0.67),
        ];
        assert_eq!(assess(&dub), Match::Different);
        // FMAB E20's: two dialogue windows of six are enough.
        let dub = [
            lag(-0.2, 0.97),
            lag(-1.6, 0.08),
            lag(-3.2, 0.69),
            lag(-1.1, 0.28),
            lag(-3.7, 0.20),
            lag(-4.9, 0.95),
        ];
        assert_eq!(assess(&dub), Match::Different);
        // K-On! OP1's AAC: a lossy copy, never far below its best.
        let copy = [
            lag(-0.3, 0.62),
            lag(-0.3, 0.64),
            lag(-0.2, 0.94),
            lag(0.0, 0.90),
        ];
        assert_eq!(assess(&copy), Match::Same(Lineup::InSync));
        // A copy with a step inside one window: that one misses, the rest say how it lines up.
        let stepped = [lag(0.0, 0.9), lag(0.0, 0.9), lag(9.0, 0.2), lag(20.0, 0.9)];
        assert_eq!(assess(&stepped), Match::Same(Lineup::Drifts));
        assert_eq!(assess(&[None, None, lag(0.0, 1.0)]), Match::TooQuiet);
        assert_eq!(
            assess(&[lag(0.0, 1.0)]),
            Match::Same(Lineup::InSync),
            "a one-window file"
        );
        assert_eq!(
            assess(&[lag(0.0, 0.2)]),
            Match::Different,
            "no miss in a window alone"
        );
        assert_eq!(assess(&[]), Match::TooQuiet);
    }

    #[test]
    fn clear_windows_say_whether_a_lag_is_steady() {
        // FMAB E01's AC-3 against its TrueHD.
        let off = [
            lag(26.7, 0.96),
            lag(26.6, 0.97),
            lag(26.6, 0.99),
            lag(26.8, 0.95),
        ];
        let Match::Same(Lineup::Off { ms }) = assess(&off) else {
            panic!()
        };
        assert!((ms - 26.65).abs() < 1e-9, "{ms}");
        // Rounding can put a window a millisecond off without anything moving.
        let rounded = [lag(0.9, 0.9), lag(-0.4, 0.9), lag(0.1, 0.9)];
        assert_eq!(assess(&rounded), Match::Same(Lineup::InSync));
        // K-On!! S00E07's AAC: steadily late, wandering over 2.2 ms.
        let wanders = [
            lag(21.0, 0.62),
            lag(20.8, 0.45),
            lag(19.5, 0.34),
            lag(21.0, 0.70),
            lag(21.7, 0.36),
            lag(21.5, 0.36),
        ];
        let Match::Same(Lineup::Off { ms }) = assess(&wanders) else {
            panic!()
        };
        assert!((ms - 21.0).abs() < 1e-9, "{ms}");
        // K-On!! S00E05's AAC: +21 ms, then +29 from 1:52 on.
        let stepped = [
            lag(20.7, 0.50),
            lag(21.7, 0.65),
            lag(20.7, 0.67),
            lag(29.2, 0.40),
        ];
        assert_eq!(assess(&stepped), Match::Same(Lineup::Drifts));
        // An unclear window does not make a drift.
        let unclear = [lag(5.0, 0.9), lag(5.1, 0.9), lag(300.0, 0.2), lag(5.0, 0.9)];
        let Match::Same(Lineup::Off { ms }) = assess(&unclear) else {
            panic!()
        };
        assert!((ms - 5.0).abs() < 1e-9, "{ms}");
    }

    fn track(id: usize, name: &str, language: &str) -> Track {
        Track {
            id,
            name: name.into(),
            language: Some(language.into()),
            language_bcp47: None,
        }
    }

    fn pair(a: usize, b: usize, lags: &[Option<Lag>]) -> Pair {
        Pair {
            a,
            b,
            lags: lags.to_vec(),
        }
    }

    #[test]
    fn a_track_is_alone_a_reference_or_reported_against_one() {
        let tracks = [
            track(1, "Japanese FLAC 2.0", "jpn"),
            track(2, "English TrueHD 5.1", "eng"),
            track(3, "English AC-3 5.1", "eng"),
            track(4, "Commentary", "eng"),
        ];
        let different = [lag(3.0, 0.05), lag(-400.0, 0.04), lag(800.0, 0.03)];
        let same = [lag(26.6, 0.97), lag(26.7, 0.98), lag(26.6, 0.96)];
        let pairs = [
            pair(1, 2, &different),
            pair(1, 3, &different),
            pair(1, 4, &different),
            pair(2, 3, &same),
            pair(2, 4, &different),
            pair(3, 4, &[None, None, lag(0.0, 0.1)]),
        ];
        let said = report(&tracks, &pairs);
        assert_eq!(said[0], (1, Said::Alone));
        assert_eq!(said[1], (2, Said::Reference));
        assert!(
            matches!(
                said[2],
                (
                    3,
                    Said::Against {
                        reference: 2,
                        lineup: Lineup::Off { .. },
                        languages: None,
                        ..
                    }
                )
            ),
            "{said:?}"
        );
        assert_eq!(said[3], (4, Said::TooQuiet));
    }

    /// FMAB E61's lags: its English dub against the Japanese, which the music and effects carry
    /// in every window; the AC-3 against the Japanese; and the AC-3 against its TrueHD.
    fn e61() -> [Vec<Option<Lag>>; 3] {
        let dub = vec![
            lag(-5.4, 0.58),
            lag(-3.2, 0.80),
            lag(-0.6, 0.44),
            lag(0.6, 0.66),
            lag(2.1, 0.37),
            lag(3.5, 0.94),
        ];
        let dub_ac3 = vec![
            lag(21.3, 0.52),
            lag(23.7, 0.77),
            lag(25.8, 0.41),
            lag(26.9, 0.63),
            lag(28.7, 0.35),
            lag(30.4, 0.92),
        ];
        let core = vec![
            lag(26.7, 0.97),
            lag(26.9, 0.98),
            lag(26.4, 0.94),
            lag(26.4, 0.99),
            lag(26.6, 0.98),
            lag(26.9, 0.97),
        ];
        [dub, dub_ac3, core]
    }

    #[test]
    fn a_track_goes_with_the_one_it_matches_best() {
        // E61's lags with one language throughout, so the dub's match counts: the AC-3 matches
        // track 1 too, and still goes with its TrueHD.
        let tracks = [
            track(1, "FLAC 2.0", "eng"),
            track(2, "TrueHD 5.1", "eng"),
            track(3, "AC-3 5.1", "eng"),
        ];
        let [dub, dub_ac3, core] = e61();
        let pairs = [pair(1, 2, &dub), pair(1, 3, &dub_ac3), pair(2, 3, &core)];
        let said = report(&tracks, &pairs);
        assert_eq!(said[0], (1, Said::Reference));
        assert!(
            matches!(
                &said[1],
                (
                    2,
                    Said::Against {
                        reference: 1,
                        lineup: Lineup::Drifts,
                        languages: None,
                        ..
                    }
                )
            ),
            "{said:?}"
        );
        assert!(
            matches!(
                said[2],
                (
                    3,
                    Said::Against {
                        reference: 2,
                        lineup: Lineup::Off { .. },
                        ..
                    }
                )
            ),
            "{said:?}"
        );
    }

    #[test]
    fn tracks_in_different_languages_match_only_as_a_copy() {
        // E61 as it is tagged: the dub no longer matches the Japanese, and the AC-3 goes with
        // its TrueHD.
        let w = [10 * S, 20 * S, 30 * S, 40 * S, 50 * S, 60 * S];
        let tracks = [
            track(1, "Japanese FLAC 2.0", "jpn"),
            track(2, "English TrueHD 5.1", "eng"),
            track(3, "English AC-3 5.1", "eng"),
        ];
        let [dub, dub_ac3, core] = e61();
        let pairs = [pair(1, 2, &dub), pair(1, 3, &dub_ac3), pair(2, 3, &core)];
        assert_eq!(
            render("e61.mkv", &tracks, &w, &pairs, false),
            "e61.mkv
  track 1  Japanese FLAC 2.0   no other track holds this audio
  track 2  English TrueHD 5.1  reference
  track 3  English AC-3 5.1    26.6 ms late against track 2
           fix: mkvmerge --sync 3:-27, or --sync 2:27 if track 2 is the one off
"
        );
        // A clean copy tagged with another language still matches, and says so: one window of
        // 0.5 is enough of a copy, one of 0.49 is not.
        let copy = |worst: f64| {
            let lags = [lag(0.0, 0.95), lag(0.0, worst), lag(0.0, 0.95)];
            let tracks = [track(1, "FLAC", "jpn"), track(2, "AAC", "eng")];
            render("c.mkv", &tracks, &w[..3], &[pair(1, 2, &lags)], false)
        };
        assert!(copy(0.5).contains("in sync with track 1\n           tagged eng, track 1 jpn\n"));
        assert!(copy(0.49).contains("  track 2  AAC   no other track holds this audio\n"));
        // Nothing says a track tagged und is in the same language: it needs a copy's match too.
        let lags = [lag(0.0, 0.95), lag(0.0, 0.3), lag(0.0, 0.95)];
        let und = [track(1, "FLAC", "jpn"), track(2, "AAC", "und")];
        let out = render("u.mkv", &und, &w[..3], &[pair(1, 2, &lags)], false);
        assert!(
            out.contains("  track 2  AAC   no other track holds this audio\n"),
            "{out}"
        );
    }

    #[test]
    fn a_files_track_reads_its_language_as_matroska_does() {
        let file_track = |language: Option<&str>, bcp47: Option<&str>| crate::mkv::Track {
            id: 1,
            number: 2,
            uid: 2,
            kind: crate::mkv::TRACK_TYPE_AUDIO,
            codec: "A_AC3".into(),
            name: None,
            language: language.unwrap_or("eng").into(),
            language_element: language.is_some(),
            language_bcp47: bcp47.map(String::from),
            default: true,
            forced: false,
            default_duration: None,
            codec_private: Vec::new(),
            encodings: Vec::new(),
            flags: crate::mkv::Flags::default(),
            video: None,
            audio: None,
        };
        // No tag at all is English, as one tagged eng is.
        let untagged = Track::of(&file_track(None, None));
        assert_eq!(untagged.language.as_deref(), Some("eng"));
        assert_eq!(untagged.name, "(eng)");
        assert_eq!(
            shown_languages(&untagged, &Track::of(&file_track(Some("eng"), None))),
            None
        );
        // Tagged only in BCP 47: Japanese, named so, and not the default English.
        let ja = Track::of(&file_track(None, Some("ja")));
        assert_eq!((ja.language.as_deref(), ja.name.as_str()), (None, "(ja)"));
    }

    #[test]
    fn languages_compare_in_one_kind_of_tag() {
        let tagged = |language: Option<&str>, bcp47: Option<&str>| Track {
            id: 0,
            name: String::new(),
            language: language.map(String::from),
            language_bcp47: bcp47.map(String::from),
        };
        let both = |iso: &str, bcp: &str| tagged(Some(iso), Some(bcp));
        let iso = |code: &str| tagged(Some(code), None);
        let bcp = |tag: &str| tagged(None, Some(tag));
        let same = |a: Track, b: Track| shown_languages(&a, &b).is_none();
        // ISO 639-2: a code's two spellings and its case.
        assert!(same(iso("fre"), iso("fra")));
        assert!(same(iso("JPN"), iso("jpn")));
        // BCP 47 when both have it: two dubs that share the old code differ; a region does not.
        // (A blank or padded tag never gets here: the parser reads it as none, or trims it.)
        assert!(!same(both("chi", "zh-yue"), both("chi", "zh-Hans")));
        assert!(same(both("eng", "en-US"), both("eng", "en-GB")));
        // Else ISO 639-2 when both have it: never `pl` against `pol`.
        assert!(same(both("pol", "pl"), iso("pol")));
        // One only in BCP 47, the other only in ISO 639-2: nothing says they are the same.
        assert!(!same(bcp("ja"), iso("jpn")));
        assert!(!same(iso("und"), iso("jpn")));
        // Said as tagged: the kind compared, or each its own.
        assert_eq!(
            shown_languages(&both("jpn", "ja"), &both("eng", "en")),
            Some(("ja".into(), "en".into()))
        );
        assert_eq!(
            shown_languages(&both("chi", "zh-yue"), &both("chi", "zh-Hans")),
            Some(("zh-yue".into(), "zh-Hans".into()))
        );
        assert_eq!(
            shown_languages(&bcp("fr"), &iso("jpn")),
            Some(("fr".into(), "jpn".into()))
        );
    }

    #[test]
    fn a_file_reads_as_one_block_with_each_fix() {
        let tracks = [
            track(1, "Main", "eng"),
            track(2, "Copy", "eng"),
            track(3, "Late copy", "eng"),
            track(4, "Stepped copy", "eng"),
            track(5, "Other", "eng"),
        ];
        let w = [10 * S, 20 * S, 30 * S, 40 * S];
        let sync = [lag(0.0, 1.0), lag(0.1, 1.0), lag(-0.1, 1.0), lag(0.0, 1.0)];
        let late = [
            lag(27.0, 1.0),
            lag(27.0, 1.0),
            lag(27.1, 1.0),
            lag(27.0, 0.2),
        ];
        let stepped = [lag(0.0, 1.0), lag(0.0, 1.0), lag(20.0, 1.0), None];
        let other = [
            lag(1.0, 0.01),
            lag(2.0, 0.01),
            lag(3.0, 0.01),
            lag(4.0, 0.01),
        ];
        // Track 2 is track 1 over again: against it, the others say what they say against 1, and
        // the first of equals is the lowest-numbered.
        let late_to_stepped = [lag(-27.0, 0.9), lag(-27.0, 0.9), lag(-7.0, 0.9), None];
        let mut pairs = vec![
            pair(1, 2, &sync),
            pair(1, 3, &late),
            pair(1, 4, &stepped),
            pair(1, 5, &other),
            pair(2, 3, &late),
            pair(2, 4, &stepped),
            pair(3, 4, &late_to_stepped),
        ];
        for a in 2..5 {
            pairs.push(pair(a, 5, &other));
        }
        assert_eq!(
            render("a.mkv", &tracks, &w, &pairs, false),
            "a.mkv
  track 1  Main          reference
  track 2  Copy          in sync with track 1
  track 3  Late copy     27.0 ms late against track 1
           fix: mkvmerge --sync 3:-27
           unclear at 0:40 (score 0.20)
  track 4  Stepped copy  drifts against track 1
           no one --sync fixes a drift; ms late by window: 0:10 +0.0  0:20 +0.0  0:30 +20.0  0:40 quiet
  track 5  Other         no other track holds this audio
"
        );
        // With only two tracks, either one may be the one to move: the sound cannot say which.
        let two = render(
            "b.mkv",
            &[tracks[0].clone(), tracks[2].clone()],
            &w,
            &pairs[1..2],
            false,
        );
        assert!(
            two.contains("fix: mkvmerge --sync 3:-27, or --sync 1:27 if track 1 is the one off"),
            "{two}"
        );
    }

    #[test]
    fn a_fix_is_given_only_where_it_lines_the_track_up_with_the_top_of_its_group() {
        let tracks = [
            track(0, "Main", "eng"),
            track(1, "Copy", "eng"),
            track(2, "Copy of the copy", "eng"),
        ];
        let w = [30 * S, 40 * S, 50 * S];
        let at = |ms: f64, score| [lag(ms, score), lag(ms, score), lag(ms, score)];
        let block = |one: [Option<Lag>; 3], two: [Option<Lag>; 3], one_two: [Option<Lag>; 3]| {
            let pairs = [pair(0, 1, &one), pair(0, 2, &two), pair(1, 2, &one_two)];
            render("a.mkv", &tracks, &w, &pairs, false)
        };
        // Track 2 is in sync with a copy 27 ms late, and matches it best: it moves with the copy.
        // It holds track 0's sound too, so track 0 moving instead is not offered.
        assert_eq!(
            block(at(27.0, 0.98), at(27.0, 0.90), at(0.0, 0.97)),
            "a.mkv
  track 0  Main              reference
  track 1  Copy              27.0 ms late against track 0
           fix: mkvmerge --sync 1:-27
  track 2  Copy of the copy  in sync with track 1
           fix: mkvmerge --sync 2:-27, the same move as track 1's
"
        );
        // 10 ms late against the late copy: no fix of its own until the copy's is made.
        assert_eq!(
            block(at(27.0, 0.98), at(37.0, 0.90), at(10.0, 0.97)),
            "a.mkv
  track 0  Main              reference
  track 1  Copy              27.0 ms late against track 0
           fix: mkvmerge --sync 1:-27
  track 2  Copy of the copy  10.0 ms late against track 1
           no fix until track 1 is fixed: check again then
"
        );
        // Against a copy in sync with track 0, a fix is a fix against track 0 too.
        assert_eq!(
            block(at(0.0, 0.98), at(10.0, 0.90), at(10.0, 0.97)),
            "a.mkv
  track 0  Main              reference
  track 1  Copy              in sync with track 0
  track 2  Copy of the copy  10.0 ms late against track 1
           fix: mkvmerge --sync 2:-10
"
        );
        // Against a drift: no --sync moves the drift, so the fix lines it up with that track, as
        // FMAB E61's AC-3 with its TrueHD (whose dub drifts against the Japanese track).
        let drifts = [lag(0.0, 0.98), lag(10.0, 0.98), lag(20.0, 0.98)];
        assert_eq!(
            block(drifts, at(5.0, 0.30), at(5.0, 0.97)),
            "a.mkv
  track 0  Main              reference
  track 1  Copy              drifts against track 0
           no one --sync fixes a drift; ms late by window: 0:30 +0.0  0:40 +10.0  0:50 +20.0
  track 2  Copy of the copy  5.0 ms late against track 1
           fix: mkvmerge --sync 2:-5
"
        );
        // Track 2 holds track 0's sound, in sync with it, but matches track 1 better: it says
        // track 0 is not the one off, so moving it instead is not offered. Track 2 waits for
        // track 1's fix, and is in sync once it is made.
        assert_eq!(
            block(at(27.0, 0.90), at(0.0, 0.85), at(-27.0, 0.98)),
            "a.mkv
  track 0  Main              reference
  track 1  Copy              27.0 ms late against track 0
           fix: mkvmerge --sync 1:-27
  track 2  Copy of the copy  27.0 ms early against track 1
           no fix until track 1 is fixed: check again then
"
        );
        // Track 2 follows track 1 and does not match track 0: moving track 0 instead would leave
        // it out of line with track 2's own move, so only track 1's fix is given.
        assert_eq!(
            block(at(27.0, 0.98), at(500.0, 0.05), at(0.0, 0.97)),
            "a.mkv
  track 0  Main              reference
  track 1  Copy              27.0 ms late against track 0
           fix: mkvmerge --sync 1:-27
  track 2  Copy of the copy  in sync with track 1
           fix: mkvmerge --sync 2:-27, the same move as track 1's
"
        );
        // With track 0 the only other track holding its sound, either may move.
        let two = render(
            "b.mkv",
            &tracks[..2],
            &w,
            &[pair(0, 1, &at(27.0, 0.98))],
            false,
        );
        assert!(
            two.contains("fix: mkvmerge --sync 1:-27, or --sync 0:27 if track 0 is the one off"),
            "{two}"
        );
    }

    #[test]
    fn what_goes_with_a_drift_is_fixed_against_it() {
        // Track 2 is steadily off against a drift: its fix lines it up with the drift. Track 3 is
        // off against track 2, which moves first.
        let tracks = [
            track(0, "Main", "eng"),
            track(1, "Drifting", "eng"),
            track(2, "Off the drift", "eng"),
            track(3, "Off that", "eng"),
        ];
        let w = [30 * S, 40 * S, 50 * S];
        let at = |ms: f64, score| [lag(ms, score), lag(ms, score), lag(ms, score)];
        let drifts = [lag(0.0, 0.98), lag(10.0, 0.98), lag(20.0, 0.98)];
        let far = at(400.0, 0.05);
        let pairs = [
            pair(0, 1, &drifts),
            pair(0, 2, &far),
            pair(0, 3, &far),
            pair(1, 2, &at(10.0, 0.97)),
            pair(1, 3, &far),
            pair(2, 3, &at(5.0, 0.97)),
        ];
        assert_eq!(
            render("a.mkv", &tracks, &w, &pairs, false),
            "a.mkv
  track 0  Main           reference
  track 1  Drifting       drifts against track 0
           no one --sync fixes a drift; ms late by window: 0:30 +0.0  0:40 +10.0  0:50 +20.0
  track 2  Off the drift  10.0 ms late against track 1
           fix: mkvmerge --sync 2:-10
  track 3  Off that       5.0 ms late against track 2
           no fix until track 2 is fixed: check again then
"
        );
    }

    #[test]
    fn a_track_another_matches_but_does_not_go_with_is_not_alone() {
        // Track 2 matches track 0 best and track 1 too, through the music: track 1 does not
        // match track 0, and goes with nothing, but its sound is in track 2.
        let tracks = [
            track(0, "English TrueHD", "eng"),
            track(1, "Japanese FLAC", "jpn"),
            track(2, "English AC-3", "eng"),
        ];
        let w = [30 * S, 40 * S, 50 * S];
        let at = |ms: f64, score| [lag(ms, score), lag(ms, score), lag(ms, score)];
        let pairs = [
            pair(0, 1, &at(300.0, 0.05)),
            pair(0, 2, &at(0.0, 0.90)),
            pair(1, 2, &at(0.0, 0.60)),
        ];
        let out = render("a.mkv", &tracks, &w, &pairs, false);
        assert!(
            out.contains("  track 1  Japanese FLAC   track 2 holds this audio too, but goes with another track\n"),
            "{out}"
        );
    }

    #[test]
    fn a_file_with_fewer_than_two_audio_tracks_says_so() {
        assert_eq!(
            render("a.mkv", &[], &[], &[], false),
            "a.mkv\n  no audio tracks\n"
        );
        assert_eq!(
            render("a.mkv", &[track(1, "Main", "eng")], &[], &[], false),
            "a.mkv\n  one audio track, nothing to compare\n"
        );
    }

    #[test]
    fn a_lag_that_rounds_to_none_has_no_minus() {
        assert_eq!(signed(-0.04, 1), "+0.0");
        assert_eq!(signed(-0.06, 1), "-0.1");
        assert_eq!(signed(20.0, 2), "+20.00");
    }

    #[test]
    fn clock_shows_hours_only_from_an_hour() {
        assert_eq!(clock(75 * S), "1:15");
        assert_eq!(clock(3725 * S), "1:02:05");
    }
}
