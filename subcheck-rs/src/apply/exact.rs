//! Whether a fixed track can be written out exactly: its header as it is, and every line's times as
//! the check after the remux expects them, where need be with a `--sync` that puts back times its
//! format cuts; and whether a file can be remuxed at all as it is. Where not, the fixes are left for
//! a hand fix before any remux, rather than refused by the check after one.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

use super::patch;
use super::remux;
use super::shift;
use super::verify;
use crate::blocks::Block;
use crate::mkv;
use crate::subs::{self, Kind};
use crate::text::timestamp_ms;

/// How a track that can be written out exactly comes back: its written times `ms` milliseconds
/// early, which the remux's `--sync` puts back (0: as written). An ASS track whose every time is the
/// same few milliseconds past the centisecond is written cut to the centisecond, each time early by
/// that much.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Early {
    pub ms: i64,
}

impl Early {
    pub const NONE: Early = Early { ms: 0 };

    fn ns(self) -> i128 {
        i128::from(self.ms) * MS
    }
}

const MS: i128 = 1_000_000;

/// Why a track cannot be written out exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unwritable {
    /// A line of a text track, this one or another, is not UTF-8: mkvmerge replaces those bytes
    /// in any remux (with U+FFFD, mkvmerge 102), so the check refuses it whichever track is fixed.
    NotUtf8 { track: usize },
    /// Its header is not UTF-8: the writer would replace those bytes (a remux alone keeps them).
    HeaderNotUtf8 { track: usize },
    /// A line's times, as written, do not read back as the check expects.
    Times { track: usize, kind: Kind, why: Why },
}

/// The first line whose times do not read back, and what about them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// A start or end (ns) finer than the format holds.
    Finer(i128),
    /// Lines that last the track's DefaultDuration (ns): written cut to the format, remuxed
    /// rounded to the millisecond.
    DefaultDuration(u64),
    /// A line (its start, ns) with no duration and no DefaultDuration, which a remux gives one.
    NoDuration(i128),
    /// A line (its start, ns) the writer does not write on its own.
    NotWritten(i128),
    /// A start or end (ns) not as far past the centisecond as most lines' starts, which a `--sync`
    /// putting that back would move.
    Past(i128, Early),
}

impl fmt::Display for Unwritable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (track, kind, why) = match self {
            Unwritable::NotUtf8 { track } => {
                return write!(
                    f,
                    "track {track} is not all UTF-8, which any remux rewrites"
                );
            }
            Unwritable::HeaderNotUtf8 { track } => {
                return write!(f, "track {track}'s header is not all UTF-8");
            }
            Unwritable::Times { track, kind, why } => (track, kind, why),
        };
        let ext = kind.extension();
        write!(f, "track {track}'s .{ext} file cannot keep its times (")?;
        match why {
            Why::Finer(ns) => {
                let precision = match kind {
                    Kind::Srt => "milliseconds",
                    Kind::Ass | Kind::Ssa => "centiseconds",
                };
                write!(f, "{} is finer than {precision}", exact(*ns))
            }
            Why::DefaultDuration(ns) => {
                write!(f, "its lines last the DefaultDuration, {} ms", millis(*ns))
            }
            Why::NoDuration(ns) => write!(f, "the line at {} has no duration", exact(*ns)),
            Why::NotWritten(ns) => {
                write!(
                    f,
                    "the line at {} would not be written as it is",
                    exact(*ns)
                )
            }
            Why::Past(ns, early) => write!(
                f,
                "{} is not {} ms past the centisecond, as most lines' starts are",
                exact(*ns),
                early.ms
            ),
        }?;
        f.write_str(")")
    }
}

/// Whether `track` can be written out exactly, each block at its times in `moved` (by block index:
/// timestamp and duration, in TimestampScale units) where it has some there, and how; then `file`,
/// the file's verdict ([`remuxable`]), which judges its lines' bytes with every other track's. A
/// track most of whose lines start the same whole milliseconds past the centisecond is held to that
/// offset, and its reason names the first time that is not that far past, or whatever else first
/// keeps it from being written so.
pub fn writable(
    track: &patch::Track,
    moved: &BTreeMap<usize, (i64, Option<u64>)>,
    file: Result<(), Unwritable>,
) -> Result<Early, Unwritable> {
    if !utf8(track.header) {
        return Err(Unwritable::HeaderNotUtf8 { track: track.id });
    }
    let default = verify::as_remuxed(track.default_duration, remux::SCALE);
    // At the times a timing fix moves them to, as they are written.
    let blocks: Cow<[Block]> = if moved.is_empty() {
        Cow::Borrowed(track.blocks)
    } else {
        let edits = patch::Edits {
            times: moved.clone(),
            ..patch::Edits::default()
        };
        Cow::Owned(patch::edited(track.kind, track.blocks, &edits))
    };
    let exact = |early| {
        blocks
            .iter()
            .try_for_each(|b| times(track, b, default, early))
    };
    let early = exact(Early::NONE).map(|()| Early::NONE).or_else(|why| {
        match remainder(track, &blocks, default) {
            Some(early) => exact(early).map(|()| early),
            None => Err(why),
        }
    });
    let early = early.map_err(|why| Unwritable::Times {
        track: track.id,
        kind: track.kind,
        why,
    })?;
    file.map(|()| early)
}

/// The whole milliseconds (`--sync` takes no less) past what the format holds that more than half
/// of `blocks` start, where that is not none: the one offset every line could share, which the
/// others break. An SRT track, which holds milliseconds, has none.
fn remainder(track: &patch::Track, blocks: &[Block], default: Option<i128>) -> Option<Early> {
    let precision = shift::precision(track.kind);
    let mut starts = [0_usize; 10];
    for b in blocks {
        let past = verify::expected(b, track.scale, default)
            .0
            .rem_euclid(precision);
        if past % MS == 0 {
            starts[usize::try_from(past / MS).expect("under 10")] += 1;
        }
    }
    (1..10_u8)
        .find(|&ms| starts[usize::from(ms)] * 2 > blocks.len())
        .map(|ms| Early { ms: i64::from(ms) })
}

/// Whether mkvmerge rewrites `track`'s lines that are not UTF-8 in a remux: any `S_TEXT` subtitle
/// track (measured on mkvmerge 102 for ASS, SRT, WebVTT, ASCII and USF, each with the same
/// warning).
pub fn rewritten(track: &mkv::Track) -> bool {
    track.kind == mkv::TRACK_TYPE_SUBTITLE && track.codec.starts_with("S_TEXT/")
}

/// Whether a file's tracks, with their blocks, can be remuxed as they are: a line that is not
/// UTF-8, in any [`rewritten`] one, comes back changed (any other is not looked at). Their headers
/// come back as they are (mkvmerge 102).
pub fn remuxable<'a>(
    tracks: impl IntoIterator<Item = (&'a mkv::Track, &'a [Block])>,
) -> Result<(), Unwritable> {
    for (t, blocks) in tracks.into_iter().filter(|(t, _)| rewritten(t)) {
        // A text track is written and checked up to a line's first NUL; any other (WebVTT, say) by
        // its whole frames.
        let ok = |b: &Block| {
            if t.is_text_subtitle() {
                utf8(&b.payload)
            } else {
                std::str::from_utf8(&b.payload).is_ok()
            }
        };
        if !blocks.iter().all(ok) {
            return Err(Unwritable::NotUtf8 { track: t.id });
        }
    }
    Ok(())
}

/// Whether bytes up to the first NUL (as a track is written and checked) are UTF-8.
fn utf8(bytes: &[u8]) -> bool {
    std::str::from_utf8(subs::c_bytes(bytes)).is_ok()
}

/// One block's times, as written and put back `early`, against what the check expects of them.
/// `default`: the track's DefaultDuration as a remux writes it.
fn times(track: &patch::Track, b: &Block, default: Option<i128>, early: Early) -> Result<(), Why> {
    let (start, duration) = verify::expected(b, track.scale, default);
    let Some(duration) = duration else {
        return Err(Why::NoDuration(start));
    };
    let Some((at, lasts)) = subs::written_times(track.kind, b, track.scale, track.default_duration)
    else {
        return Err(Why::NotWritten(start));
    };
    let written = (at + early.ns(), lasts);
    if written == (start, duration) {
        return Ok(());
    }
    let off = if written.0 == start {
        start + duration
    } else {
        start
    };
    Err(match track.default_duration {
        Some(dd) if b.duration.is_none() && written.0 == start => Why::DefaultDuration(dd),
        _ if early == Early::NONE => Why::Finer(off),
        _ => Why::Past(off, early),
    })
}

/// A time (ns) as `h:mm:ss.mmm`, and the digits past the millisecond where it has any; one before 0
/// with a minus sign.
fn exact(ns: i128) -> String {
    let sign = if ns < 0 { "-" } else { "" };
    let ns = ns.abs();
    let ms = i64::try_from(ns / 1_000_000).unwrap_or(i64::MAX);
    let mut out = format!("{sign}{}", timestamp_ms(ms));
    let rest = ns % 1_000_000;
    if rest != 0 {
        out.push_str(format!("{rest:06}").trim_end_matches('0'));
    }
    out
}

/// A duration (ns) in milliseconds, with the decimals it has.
fn millis(ns: u64) -> String {
    let (ms, rest) = (ns / 1_000_000, ns % 1_000_000);
    if rest == 0 {
        ms.to_string()
    } else {
        format!("{ms}.{}", format!("{rest:06}").trim_end_matches('0'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(ms: i64, duration: Option<u64>) -> Block {
        Block {
            track: 1,
            position: 0,
            timestamp: ms,
            duration,
            payload: b"0,0,D,,0,0,0,,a line".to_vec(),
        }
    }

    fn track(kind: Kind, blocks: &[Block], default_duration: Option<u64>) -> patch::Track<'_> {
        patch::Track {
            id: 1,
            kind,
            blocks,
            header: b"[Script Info]\n",
            scale: 1_000_000,
            default_duration,
            timing: patch::Timing::AsScanned,
            writable: Ok(Early::NONE),
        }
    }

    /// The verdict on one block's track: how many ms early it is written, or the reason.
    fn verdict(kind: Kind, b: Block, default_duration: Option<u64>) -> Result<i64, String> {
        lines(kind, &[b], default_duration)
    }

    /// The verdict on a track of `blocks`, as [`verdict`] gives it.
    fn lines(kind: Kind, blocks: &[Block], default_duration: Option<u64>) -> Result<i64, String> {
        writable(
            &track(kind, blocks, default_duration),
            &BTreeMap::new(),
            Ok(()),
        )
        .map(|e| e.ms)
        .map_err(|e| e.to_string())
    }

    const FRAME: Option<u64> = Some(41_708_333);

    #[test]
    fn times_the_format_keeps_pass() {
        assert_eq!(verdict(Kind::Ass, block(1000, Some(1500)), None), Ok(0));
        assert_eq!(verdict(Kind::Ssa, block(1000, Some(1500)), None), Ok(0));
        assert_eq!(verdict(Kind::Srt, block(1007, Some(1499)), None), Ok(0));
        // 25 fps lines last 40 ms, which both keep; 2 s lines too.
        assert_eq!(verdict(Kind::Ass, block(0, None), Some(40_000_000)), Ok(0));
        assert_eq!(
            verdict(Kind::Srt, block(0, None), Some(2_000_000_000)),
            Ok(0)
        );
        // A line of no length: a remux writes it a BlockDuration of 0 (mkvmerge 102).
        assert_eq!(verdict(Kind::Ass, block(1000, Some(0)), None), Ok(0));
    }

    #[test]
    fn a_time_finer_than_the_format_is_said_with_its_track() {
        // 7 ms past the centisecond, after a line on it.
        let blocks = [block(1000, Some(1500)), block(3007, Some(1000))];
        assert_eq!(
            lines(Kind::Ass, &blocks, None),
            Err(
                "track 1's .ass file cannot keep its times (0:00:03.007 is finer than centiseconds)"
                    .into()
            )
        );
        // Its end, where its start is kept.
        assert_eq!(
            verdict(Kind::Ass, block(1000, Some(1499)), None),
            Err(
                "track 1's .ass file cannot keep its times (0:00:02.499 is finer than centiseconds)"
                    .into()
            )
        );
        // A TimestampScale of 100 µs: 1000.4 ms.
        let blocks = [block(10_004, Some(5000))];
        let t = patch::Track {
            scale: 100_000,
            ..track(Kind::Srt, &blocks, None)
        };
        assert_eq!(
            writable(&t, &BTreeMap::new(), Ok(())).map_err(|e| e.to_string()),
            Err(
                "track 1's .srt file cannot keep its times (0:00:01.0004 is finer than milliseconds)"
                    .into()
            )
        );
    }

    #[test]
    fn lines_lasting_a_default_duration_the_format_cannot_keep_say_so() {
        // 23.976 fps: a remux makes each line 42 ms; the .ass says 40, the .srt 41.
        for (kind, ext) in [(Kind::Ass, "ass"), (Kind::Srt, "srt")] {
            assert_eq!(
                verdict(kind, block(0, None), FRAME),
                Err(format!(
                    "track 1's .{ext} file cannot keep its times (its lines last the DefaultDuration, 41.708333 ms)"
                ))
            );
        }
        // A line with a BlockDuration of its own is held to that, not to the DefaultDuration.
        assert_eq!(verdict(Kind::Srt, block(0, Some(42)), FRAME), Ok(0));
    }

    #[test]
    fn a_line_with_no_duration_or_not_written_on_its_own_says_so() {
        assert_eq!(
            verdict(Kind::Ass, block(3000, None), None),
            Err(
                "track 1's .ass file cannot keep its times (the line at 0:00:03.000 has no duration)"
                    .into()
            )
        );
        // A line before 0, said as such.
        assert_eq!(
            verdict(Kind::Ass, block(-50, Some(1000)), None),
            Err(
                "track 1's .ass file cannot keep its times (the line at -0:00:00.050 would not be written as it is)"
                    .into()
            )
        );
        // An SRT line of no length waits for the next block.
        assert_eq!(
            verdict(Kind::Srt, block(1000, Some(0)), None),
            Err(
                "track 1's .srt file cannot keep its times (the line at 0:00:01.000 would not be written as it is)"
                    .into()
            )
        );
    }

    #[test]
    fn a_header_that_is_not_utf8_cannot_be_written() {
        let latin1 = [b"Style: Caf".as_slice(), &[0xE9]].concat();
        let blocks = [block(0, Some(1000))];
        let t = patch::Track {
            header: &latin1,
            ..track(Kind::Ass, &blocks, None)
        };
        let why = Unwritable::HeaderNotUtf8 { track: 1 };
        assert_eq!(writable(&t, &BTreeMap::new(), Ok(())), Err(why));
        assert_eq!(why.to_string(), "track 1's header is not all UTF-8");
        // Its lines are the file's to judge: `remuxable`.
        let mut cafe = block(1000, Some(1000));
        cafe.payload.push(0xE9);
        let blocks = [block(0, Some(1000)), cafe];
        assert_eq!(
            writable(&track(Kind::Ass, &blocks, None), &BTreeMap::new(), Ok(())),
            Ok(Early::NONE)
        );
    }

    /// A subtitle track of `codec`, as the file's track `id`.
    fn subtitle(id: usize, codec: &str) -> mkv::Track {
        mkv::Track {
            id,
            number: id as u64 + 1,
            uid: id as u64 + 1,
            kind: mkv::TRACK_TYPE_SUBTITLE,
            codec: codec.into(),
            name: None,
            language: "eng".into(),
            language_element: true,
            language_bcp47: None,
            default: false,
            forced: false,
            default_duration: None,
            codec_private: Vec::new(),
            encodings: Vec::new(),
            flags: mkv::Flags::default(),
            video: None,
            audio: None,
        }
    }

    #[test]
    fn a_file_with_a_line_that_is_not_utf8_in_any_text_track_cannot_be_remuxed() {
        let mut cafe = block(1000, Some(1000));
        cafe.payload.push(0xE9);
        let fine = [block(0, Some(1000))];
        let latin1 = [block(0, Some(1000)), cafe];
        let (ass, srt) = (subtitle(2, "S_TEXT/ASS"), subtitle(3, "S_TEXT/UTF8"));
        assert_eq!(remuxable([(&ass, &fine[..]), (&srt, &fine[..])]), Ok(()));
        assert_eq!(
            remuxable([(&ass, &fine[..]), (&srt, &latin1[..])]),
            Err(Unwritable::NotUtf8 { track: 3 })
        );
        assert_eq!(
            Unwritable::NotUtf8 { track: 3 }.to_string(),
            "track 3 is not all UTF-8, which any remux rewrites"
        );
        // Past a NUL, nothing of a text track is written or checked; WebVTT is checked by its
        // whole frames.
        let mut padded = block(0, Some(1000));
        padded.payload.extend_from_slice(&[0, 0xE9]);
        let padded = [padded];
        assert_eq!(remuxable([(&ass, &padded[..])]), Ok(()));
        let vtt = subtitle(4, "S_TEXT/WEBVTT");
        assert_eq!(
            remuxable([(&vtt, &padded[..])]),
            Err(Unwritable::NotUtf8 { track: 4 })
        );
    }

    #[test]
    fn the_tracks_whose_lines_a_remux_rewrites_are_the_s_text_subtitles() {
        for codec in [
            "S_TEXT/ASS",
            "S_TEXT/SSA",
            "S_TEXT/UTF8",
            "S_TEXT/WEBVTT",
            "S_TEXT/USF",
            "S_TEXT/ASCII",
        ] {
            assert!(rewritten(&subtitle(0, codec)), "{codec}");
        }
        for codec in ["S_HDMV/PGS", "S_VOBSUB"] {
            assert!(!rewritten(&subtitle(0, codec)), "{codec}");
        }
        let not_subtitles = mkv::Track {
            kind: mkv::TRACK_TYPE_VIDEO,
            ..subtitle(0, "S_TEXT/WEBVTT")
        };
        assert!(!rewritten(&not_subtitles));
        // A track it does not rewrite is not looked at: a PGS frame is no text.
        let image = [Block {
            payload: vec![0x50, 0x47, 0xFF, 0xFE],
            ..block(0, Some(1000))
        }];
        assert_eq!(
            remuxable([(&subtitle(5, "S_HDMV/PGS"), &image[..])]),
            Ok(())
        );
    }

    #[test]
    fn the_files_verdict_follows_the_tracks_own() {
        let other = Unwritable::NotUtf8 { track: 3 };
        let fine = [block(1000, Some(1000))];
        let t = track(Kind::Ass, &fine, None);
        assert_eq!(writable(&t, &BTreeMap::new(), Err(other)), Err(other));
        let finer = [block(1007, Some(1000)), block(2003, Some(1000))];
        let t = track(Kind::Ass, &finer, None);
        assert!(matches!(
            writable(&t, &BTreeMap::new(), Err(other)),
            Err(Unwritable::Times { .. })
        ));
        // One written early: the file's verdict still holds.
        let early = [block(1007, Some(1000))];
        let t = track(Kind::Ass, &early, None);
        assert_eq!(writable(&t, &BTreeMap::new(), Err(other)), Err(other));
    }

    #[test]
    fn a_moved_line_is_held_to_its_new_times() {
        // A line 7 ms off the centisecond, moved onto one: it can be written. Left, it cannot.
        let blocks = [block(1000, Some(1000)), block(2047, Some(1000))];
        let t = track(Kind::Ass, &blocks, None);
        let moved = BTreeMap::from([(1, (2000, Some(1000)))]);
        assert_eq!(writable(&t, &moved, Ok(())), Ok(Early::NONE));
        let moved = BTreeMap::from([(0, (960, Some(1000)))]);
        assert!(writable(&t, &moved, Ok(())).is_err());
    }

    #[test]
    fn a_track_whose_every_time_is_the_same_whole_ms_past_the_centisecond_is_written_that_early() {
        let at7 = [
            block(1007, Some(1500)),
            block(12_347, Some(1660)),
            // No length: both its ends are 7 ms past.
            block(20_007, Some(0)),
            block(30_007, Some(1000)),
        ];
        assert_eq!(lines(Kind::Ass, &at7, None), Ok(7));
        assert_eq!(lines(Kind::Ssa, &at7, None), Ok(7));
        // 3 ms past, at 100 µs units.
        let blocks = [block(10_030, Some(15_000)), block(20_030, Some(9_000))];
        let t = patch::Track {
            scale: 100_000,
            ..track(Kind::Ass, &blocks, None)
        };
        assert_eq!(writable(&t, &BTreeMap::new(), Ok(())), Ok(Early { ms: 3 }));
    }

    #[test]
    fn a_track_not_all_one_whole_ms_offset_past_the_centisecond_says_which_line_breaks_it() {
        let reason = |why: &str| Err(format!("track 1's .ass file cannot keep its times ({why})"));
        let off = |at: &str| {
            reason(&format!(
                "{at} is not 7 ms past the centisecond, as most lines' starts are"
            ))
        };
        // 7 ms past, but one line 3.
        let mut mixed: Vec<Block> = (1..=99).map(|s| block(s * 1000 + 7, Some(1000))).collect();
        mixed[49] = block(50_003, Some(1000));
        assert_eq!(lines(Kind::Ass, &mixed, None), off("0:00:50.003"));
        // One on the centisecond.
        mixed[49] = block(50_000, Some(1000));
        assert_eq!(lines(Kind::Ass, &mixed, None), off("0:00:50.000"));
        // A start 7 ms past whose end is on the centisecond.
        let end = [block(1007, Some(1493))];
        assert_eq!(lines(Kind::Ass, &end, None), off("0:00:02.500"));
        // A line 7 ms past with no duration, which a remux gives one.
        let none = [block(1007, Some(1500)), block(3007, None)];
        assert_eq!(
            lines(Kind::Ass, &none, None),
            reason("the line at 0:00:03.007 has no duration")
        );
        // Lines 7 ms past lasting a 23.976 fps frame.
        assert_eq!(
            verdict(Kind::Ass, block(1007, None), FRAME),
            reason("its lines last the DefaultDuration, 41.708333 ms")
        );
    }

    #[test]
    fn a_track_most_of_whose_lines_start_no_one_whole_ms_offset_past_the_centisecond_keeps_its_reason()
     {
        let finer = |ext: &str, at: &str, precision: &str| {
            Err(format!(
                "track 1's .{ext} file cannot keep its times ({at} is finer than {precision})"
            ))
        };
        // A first line on the centisecond, a later one 7 ms past: half each.
        let later = [block(1000, Some(1000)), block(2007, Some(1000))];
        assert_eq!(
            lines(Kind::Ass, &later, None),
            finer("ass", "0:00:02.007", "centiseconds")
        );
        // A first line 7 ms past, the rest on the centisecond.
        let first = [
            block(1007, Some(1000)),
            block(2000, Some(1000)),
            block(3000, Some(1000)),
            block(4000, Some(1000)),
        ];
        assert_eq!(
            lines(Kind::Ass, &first, None),
            finer("ass", "0:00:01.007", "centiseconds")
        );
        // 7.4 ms past: `--sync` takes whole ms.
        let blocks = [block(10_074, Some(15_000))];
        let t = patch::Track {
            scale: 100_000,
            ..track(Kind::Ass, &blocks, None)
        };
        assert_eq!(
            writable(&t, &BTreeMap::new(), Ok(()))
                .map(|e| e.ms)
                .map_err(|e| e.to_string()),
            finer("ass", "0:00:01.0074", "centiseconds")
        );
        // SRT holds milliseconds: no whole-ms offset is any use to it.
        assert_eq!(
            verdict(Kind::Srt, block(1007, None), FRAME),
            Err("track 1's .srt file cannot keep its times (its lines last the DefaultDuration, 41.708333 ms)".into())
        );
    }

    #[test]
    fn a_timing_fix_moving_some_lines_of_a_track_written_early_leaves_two_offsets() {
        let blocks = [block(1007, Some(1000)), block(2007, Some(1000))];
        let t = track(Kind::Ass, &blocks, None);
        // Moved onto the centisecond, one line: the other is still 7 ms past.
        let moved = BTreeMap::from([(1, (1960, Some(1000)))]);
        assert!(matches!(
            writable(&t, &moved, Ok(())),
            Err(Unwritable::Times { .. })
        ));
        // Every line: none is past.
        let moved = BTreeMap::from([(0, (960, Some(1000))), (1, (1960, Some(1000)))]);
        assert_eq!(writable(&t, &moved, Ok(())), Ok(Early::NONE));
        // Two of four: half are on it, so the track is held to no offset, judged at its moved times.
        let blocks = [
            block(1007, Some(1000)),
            block(2007, Some(1000)),
            block(3007, Some(1000)),
            block(4007, Some(1000)),
        ];
        let t = track(Kind::Ass, &blocks, None);
        let moved = BTreeMap::from([(2, (2460, Some(1000))), (3, (960, Some(1000)))]);
        assert_eq!(
            writable(&t, &moved, Ok(())).map_err(|e| e.to_string()),
            Err(
                "track 1's .ass file cannot keep its times (0:00:01.007 is finer than centiseconds)"
                    .into()
            )
        );
        // Three of four 7 ms past: held to that, the moved line breaks it.
        let moved = BTreeMap::from([(3, (960, Some(1000)))]);
        assert_eq!(
            writable(&t, &moved, Ok(())).map_err(|e| e.to_string()),
            Err("track 1's .ass file cannot keep its times (0:00:00.960 is not 7 ms past the centisecond, as most lines' starts are)".into())
        );
    }
}
