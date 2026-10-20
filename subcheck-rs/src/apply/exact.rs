//! Whether a fixed track can be written out exactly: its header as it is, and every line's times as
//! the check after the remux expects them; and whether a file can be remuxed at all as it is. Where
//! not, the fixes are left for a hand fix before any remux, rather than refused by the check after
//! one.

use std::collections::BTreeMap;
use std::fmt;

use super::patch;
use super::remux;
use super::verify;
use crate::blocks::Block;
use crate::mkv;
use crate::subs::{self, Kind};
use crate::text::timestamp_ms;

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
        }?;
        f.write_str(")")
    }
}

/// Whether `track` can be written out exactly, each block at its times in `moved` (by block index:
/// timestamp and duration, in TimestampScale units) where it has some there; then `file`, the
/// file's verdict ([`remuxable`]), which judges its lines' bytes with every other track's.
pub fn writable(
    track: &patch::Track,
    moved: &BTreeMap<usize, (i64, Option<u64>)>,
    file: Result<(), Unwritable>,
) -> Result<(), Unwritable> {
    if !utf8(track.header) {
        return Err(Unwritable::HeaderNotUtf8 { track: track.id });
    }
    let default = verify::as_remuxed(track.default_duration, remux::SCALE);
    for (i, b) in track.blocks.iter().enumerate() {
        let at;
        let b = match moved.get(&i) {
            Some(&(timestamp, duration)) => {
                at = Block {
                    timestamp,
                    duration,
                    ..b.clone()
                };
                &at
            }
            None => b,
        };
        times(track, b, default).map_err(|why| Unwritable::Times {
            track: track.id,
            kind: track.kind,
            why,
        })?;
    }
    file
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

/// One block's times, as written, against what the check expects of them. `default`: the track's
/// DefaultDuration as a remux writes it.
fn times(track: &patch::Track, b: &Block, default: Option<i128>) -> Result<(), Why> {
    let (start, duration) = verify::expected(b, track.scale, default);
    let Some(duration) = duration else {
        return Err(Why::NoDuration(start));
    };
    let Some(written) = subs::written_times(track.kind, b, track.scale, track.default_duration)
    else {
        return Err(Why::NotWritten(start));
    };
    if written == (start, duration) {
        return Ok(());
    }
    Err(match track.default_duration {
        Some(dd) if b.duration.is_none() && written.0 == start => Why::DefaultDuration(dd),
        _ if written.0 != start => Why::Finer(start),
        _ => Why::Finer(start + duration),
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
            writable: Ok(()),
        }
    }

    /// The verdict on one block's track, as its reason.
    fn verdict(kind: Kind, b: Block, default_duration: Option<u64>) -> Result<(), String> {
        let blocks = [b];
        writable(
            &track(kind, &blocks, default_duration),
            &BTreeMap::new(),
            Ok(()),
        )
        .map_err(|e| e.to_string())
    }

    const FRAME: Option<u64> = Some(41_708_333);

    #[test]
    fn times_the_format_keeps_pass() {
        assert_eq!(verdict(Kind::Ass, block(1000, Some(1500)), None), Ok(()));
        assert_eq!(verdict(Kind::Ssa, block(1000, Some(1500)), None), Ok(()));
        assert_eq!(verdict(Kind::Srt, block(1007, Some(1499)), None), Ok(()));
        // 25 fps lines last 40 ms, which both keep; 2 s lines too.
        assert_eq!(verdict(Kind::Ass, block(0, None), Some(40_000_000)), Ok(()));
        assert_eq!(
            verdict(Kind::Srt, block(0, None), Some(2_000_000_000)),
            Ok(())
        );
        // A line of no length: a remux writes it a BlockDuration of 0 (mkvmerge 102).
        assert_eq!(verdict(Kind::Ass, block(1000, Some(0)), None), Ok(()));
    }

    #[test]
    fn a_time_finer_than_the_format_is_said_with_its_track() {
        assert_eq!(
            verdict(Kind::Ass, block(1007, Some(1500)), None),
            Err(
                "track 1's .ass file cannot keep its times (0:00:01.007 is finer than centiseconds)"
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
        assert_eq!(verdict(Kind::Srt, block(0, Some(42)), FRAME), Ok(()));
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
            Ok(())
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
        let finer = [block(1007, Some(1000))];
        let t = track(Kind::Ass, &finer, None);
        assert!(matches!(
            writable(&t, &BTreeMap::new(), Err(other)),
            Err(Unwritable::Times { .. })
        ));
    }

    #[test]
    fn a_moved_line_is_held_to_its_new_times() {
        // A line 7 ms off the centisecond, moved onto one: it can be written. Left, it cannot.
        let blocks = [block(1000, Some(1000)), block(2047, Some(1000))];
        let t = track(Kind::Ass, &blocks, None);
        let moved = BTreeMap::from([(1, (2000, Some(1000)))]);
        assert_eq!(writable(&t, &moved, Ok(())), Ok(()));
        let moved = BTreeMap::from([(0, (960, Some(1000)))]);
        assert!(writable(&t, &moved, Ok(())).is_err());
    }
}
