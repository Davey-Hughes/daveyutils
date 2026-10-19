//! Subtitle tracks written out exactly as mkvextract writes them.
//!
//! The rules follow mkvtoolnix's `src/extract/xtr_textsubs.cpp` and `mm_io_c::puts`, and the
//! oracle tests hold them to mkvextract's real output byte for byte. Where a rule looks odd (end
//! times truncated per part for ASS but as a whole for SRT), it is mkvextract's.

use std::io::{self, Write};

use crate::blocks::{self, Block, ReadPath};
use crate::ebml::{Error, Reader, Result};
use crate::mkv::{self, Layout, Track};

const BOM: &[u8] = b"\xEF\xBB\xBF";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ass,
    Ssa,
    Srt,
}

impl Kind {
    pub fn of(track: &Track) -> Option<Kind> {
        match track.codec.as_str() {
            "S_TEXT/ASS" => Some(Kind::Ass),
            "S_TEXT/SSA" => Some(Kind::Ssa),
            "S_TEXT/UTF8" => Some(Kind::Srt),
            _ => None,
        }
    }

    /// The extension mkvextract's output would have.
    pub fn extension(self) -> &'static str {
        match self {
            Kind::Ass => "ass",
            Kind::Ssa => "ssa",
            Kind::Srt => "srt",
        }
    }
}

#[derive(Debug)]
pub struct Extracted {
    pub kind: Kind,
    pub path: ReadPath,
    pub events: usize,
}

/// Write track `track` of the file behind `r`/`layout` to `out` the way mkvextract would.
pub fn extract(
    r: &Reader,
    layout: &Layout,
    track: &Track,
    force_walk: bool,
    out: &mut impl Write,
) -> Result<Extracted> {
    let Some(kind) = Kind::of(track) else {
        return Err(Error::Unsupported(format!(
            "track {} is {}, not a text subtitle track",
            track.id, track.codec
        )));
    };
    let read = blocks::read_tracks(r, layout, &[track.number], force_walk)?
        .pop()
        .expect("read_tracks returns one entry per track asked for");
    let events = write_track(track, kind, &read.blocks, layout.timestamp_scale, out)?;
    Ok(Extracted {
        kind,
        path: read.path,
        events,
    })
}

/// A track written as `extract` writes it, from its blocks: all of them, in file order, payloads
/// decoded. The number of events written.
pub fn write_track(
    track: &Track,
    kind: Kind,
    blocks: &[Block],
    scale: u64,
    out: &mut impl Write,
) -> Result<usize> {
    Ok(match kind {
        Kind::Ass | Kind::Ssa => {
            let private = blocks::decode(
                track.codec_private.clone(),
                &track.encodings,
                mkv::SCOPE_CODEC_PRIVATE,
            )?;
            let writer = AssWriter::new(&private, kind == Kind::Ssa);
            let lines: Vec<(i64, String)> = blocks
                .iter()
                .filter_map(|b| writer.dialogue(b, scale, track.default_duration))
                .collect();
            let written = lines.len();
            writer.write(lines, out)?;
            written
        }
        Kind::Srt => write_srt(blocks, scale, track.default_duration, out)?,
    })
}

/// An ASS/SSA script header, prepared the way mkvextract prepares CodecPrivate.
pub struct AssWriter {
    header: String,
    /// Sections that followed `[Events]` in CodecPrivate; mkvextract moves them to the end.
    post_events: String,
    /// The `[Events]` Format line's field names, lowercased.
    format: Vec<String>,
    /// Comma-separated fields in a block: ReadOrder plus every format field but Start and End.
    num_fields: usize,
}

impl AssWriter {
    pub fn new(codec_private: &[u8], is_ssa: bool) -> AssWriter {
        let private = codec_private.strip_prefix(BOM).unwrap_or(codec_private);
        let mut s = c_string(private);
        let has_format = s
            .find("[Events]")
            .is_some_and(|i| s[i..].contains("Format:"));
        if !has_format {
            s.push_str(if is_ssa {
                "\n[Events]\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n"
            } else {
                "\n[Events]\nFormat: Layer, Start, End, Style, Actor, MarginL, MarginR, MarginV, Effect, Text\n"
            });
        }
        let kept = s.trim_end_matches(['\0', ' ', '\t', '\r', '\n']).len();
        s.truncate(kept);
        s.push('\n');

        let events = s.find("[Events]").unwrap_or_default();
        let format_at = events + s[events..].find("Format:").unwrap_or_default();
        let mut line_end = s[format_at..].find('\n').map_or(s.len(), |i| format_at + i);
        let mut format_line = s[format_at + 7..line_end].to_ascii_lowercase();
        if !format_line.contains("text") {
            if format_line.ends_with('\r') {
                format_line.pop();
                line_end -= 1;
            }
            format_line.push_str(",text");
            s.insert_str(line_end, ", Text");
        }
        let format: Vec<String> = split(&format_line, ",", usize::MAX)
            .into_iter()
            .map(|f| f.trim_matches(['\0', ' ', '\t', '\r', '\n']).to_string())
            .collect();
        let num_fields = 1 + format
            .iter()
            .filter(|f| *f != "start" && *f != "end")
            .count();

        let mut post_events = String::new();
        if let Some(i) = s[events..].find("\n[") {
            post_events = s[events + i..].to_string();
            s.truncate(events + i + 1);
        }
        AssWriter {
            header: s,
            post_events,
            format,
            num_fields,
        }
    }

    /// One block's `Dialogue:` line and its ReadOrder; `None` when the ReadOrder is not an integer
    /// (mkvextract skips those lines). `default_duration` is the track's, in nanoseconds.
    pub fn dialogue(
        &self,
        block: &Block,
        scale: u64,
        default_duration: Option<u64>,
    ) -> Option<(i64, String)> {
        let (start, end) = ass_ms(block, scale, default_duration);
        let mut fields = split(&c_string(&block.payload), ",", self.num_fields);
        fields.resize(self.num_fields, String::new());
        let read_order = read_order(&block.payload)?;

        let mut line = String::from("Dialogue: ");
        let mut idx = 1;
        for (i, name) in self.format.iter().enumerate() {
            if i > 0 {
                line.push(',');
            }
            match name.as_str() {
                "marked" => {
                    let v = fields
                        .get(idx)
                        .filter(|v| !v.is_empty())
                        .map_or("0", String::as_str);
                    line.push_str("Marked=");
                    line.push_str(v);
                    idx += 1;
                }
                "start" => line.push_str(&ass_time(start)),
                "end" => line.push_str(&ass_time(end)),
                _ => {
                    if idx < self.num_fields {
                        line.push_str(&fields[idx]);
                        idx += 1;
                    }
                }
            }
        }
        line.push('\n');
        Some((read_order, line))
    }

    /// The whole file: BOM, header, lines in ReadOrder, then any sections that followed `[Events]`.
    pub fn write(&self, mut lines: Vec<(i64, String)>, out: &mut impl Write) -> io::Result<()> {
        lines.sort_by_key(|(read_order, _)| *read_order);
        out.write_all(BOM)?;
        puts(out, &self.header)?;
        for (_, line) in &lines {
            puts(out, line)?;
        }
        puts(out, &self.post_events)
    }
}

/// An SRT file from S_TEXT/UTF8 blocks in file order, returning the number of entries written. An
/// entry with no duration waits for the next block and takes the gap to it as its duration, as in
/// mkvextract.
pub fn write_srt(
    blocks: &[Block],
    scale: u64,
    default_duration: Option<u64>,
    out: &mut impl Write,
) -> io::Result<usize> {
    out.write_all(BOM)?;
    let mut count = 0u64;
    let mut pending: Option<(i128, String)> = None;
    for b in blocks {
        let start = ns(b.timestamp, scale);
        if let Some((at, text)) = pending.take()
            && !text.is_empty()
        {
            count += 1;
            srt_entry(out, count, at, (start - at).abs(), &text)?;
        }
        let duration = duration_ns(b, scale, default_duration);
        let text = srt_text(b);
        if srt_alone(duration, &text) {
            count += 1;
            srt_entry(out, count, start, duration, &text)?;
        } else {
            pending = Some((start, text));
        }
    }
    // A last entry still waiting has no next block to take a duration from, and is dropped. That
    // is mkvextract's behaviour too: xtr_srt_c flushes only from handle_frame, and its
    // finish_track/finish_file are the base class's empty ones. Not a bug to fix here.
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

fn srt_entry(
    out: &mut impl Write,
    n: u64,
    start_ns: i128,
    duration_ns: i128,
    text: &str,
) -> io::Result<()> {
    let (start, end) = srt_ms(start_ns, duration_ns);
    puts(
        out,
        &format!("{n}\n{} --> {}\n{text}\n\n", srt_time(start), srt_time(end)),
    )
}

fn ns(timestamp: i64, scale: u64) -> i128 {
    i128::from(timestamp) * i128::from(scale)
}

/// BlockDuration, else the track's DefaultDuration, else -1: mkvextract's "missing" value, which
/// its integer arithmetic then turns into a zero-length (ASS) or one-nanosecond-short (SRT) entry.
fn duration_ns(block: &Block, scale: u64, default_duration: Option<u64>) -> i128 {
    block
        .duration
        .map(|d| i128::from(d) * i128::from(scale))
        .or(default_duration.map(i128::from))
        .unwrap_or(-1)
}

/// An ASS or SSA line's start and end in whole milliseconds, before the cut to centiseconds:
/// mkvextract cuts the start and the duration to the millisecond each, then adds them.
fn ass_ms(block: &Block, scale: u64, default_duration: Option<u64>) -> (i128, i128) {
    let start = ns(block.timestamp, scale) / 1_000_000;
    (
        start,
        start + duration_ns(block, scale, default_duration) / 1_000_000,
    )
}

/// An SRT entry's start and end in whole milliseconds: each cut as a whole.
fn srt_ms(start_ns: i128, duration_ns: i128) -> (i128, i128) {
    (start_ns / 1_000_000, (start_ns + duration_ns) / 1_000_000)
}

/// An SRT block's text as written: up to its first NUL, without CRs.
fn srt_text(block: &Block) -> String {
    c_string(&block.payload).replace('\r', "")
}

/// Whether an SRT block is written as an entry of its own: one of no length waits for the next
/// block, and one of no text is never written.
fn srt_alone(duration_ns: i128, text: &str) -> bool {
    duration_ns != 0 && !text.is_empty()
}

/// What a block's line, as [`write_track`] writes it, reads back as in mkvmerge: its start and
/// duration in nanoseconds, at the format's precision (centiseconds for ASS and SSA, milliseconds
/// for SRT). `None` where it comes back as no line of its own: an ASS or SSA block whose ReadOrder
/// is no integer, an SRT block of no length or no text (none is written), or a time before 0
/// (written, by mkvextract's arithmetic, as no time at all).
pub fn written_times(
    kind: Kind,
    block: &Block,
    scale: u64,
    default_duration: Option<u64>,
) -> Option<(i128, i128)> {
    let (start, end, cut) = match kind {
        Kind::Ass | Kind::Ssa => {
            read_order(&block.payload)?;
            let (start, end) = ass_ms(block, scale, default_duration);
            (start, end, 10)
        }
        Kind::Srt => {
            let duration = duration_ns(block, scale, default_duration);
            if !srt_alone(duration, &srt_text(block)) {
                return None;
            }
            let (start, end) = srt_ms(ns(block.timestamp, scale), duration);
            (start, end, 1)
        }
    };
    if start < 0 || end < 0 {
        return None;
    }
    let (start, end) = (start / cut * cut, end / cut * cut);
    Some((start * 1_000_000, (end - start) * 1_000_000))
}

fn ass_time(ms: i128) -> String {
    format!(
        "{}:{:02}:{:02}.{:02}",
        ms / 1000 / 60 / 60,
        (ms / 1000 / 60) % 60,
        (ms / 1000) % 60,
        (ms % 1000) / 10
    )
}

fn srt_time(ms: i128) -> String {
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 1000 / 60 / 60,
        (ms / 1000 / 60) % 60,
        (ms / 1000) % 60,
        ms % 1000
    )
}

/// An ASS or SSA block's ReadOrder, its first field; `None` when that is no number (mkvextract
/// leaves such a block out).
pub(crate) fn read_order(payload: &[u8]) -> Option<i64> {
    let field = c_bytes(payload).split(|&b| b == b',').next()?;
    std::str::from_utf8(field).ok()?.parse().ok()
}

/// Bytes up to the first NUL, as text: mkvextract handles these as C strings.
pub(crate) fn c_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(c_bytes(bytes)).into_owned()
}

/// Bytes up to the first NUL.
pub(crate) fn c_bytes(bytes: &[u8]) -> &[u8] {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    &bytes[..end]
}

/// mkvtoolnix's `split`: at most `max` parts, the last holding the rest, and no empty part after a
/// trailing separator.
fn split(text: &str, sep: &str, max: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut consumed = 0;
    while out.len() + 1 < max {
        let Some(i) = text[consumed..].find(sep) else {
            break;
        };
        out.push(text[consumed..consumed + i].to_string());
        consumed += i + sep.len();
    }
    if consumed < text.len() {
        out.push(text[consumed..].to_string());
    }
    out
}

/// mkvtoolnix's `puts` on Unix: a CR directly before an LF is dropped, and output stops at a NUL.
fn puts(out: &mut impl Write, s: &str) -> io::Result<()> {
    let s = s.split('\0').next().unwrap_or_default();
    out.write_all(s.replace("\r\n", "\n").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(timestamp: i64, duration: Option<u64>, payload: &str) -> Block {
        Block {
            track: 1,
            position: 0,
            timestamp,
            duration,
            payload: payload.as_bytes().to_vec(),
        }
    }

    const HEADER: &str = "[Script Info]\r\nScriptType: v4.00+\r\n\r\n[Events]\r\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\n\r\n";

    #[test]
    fn header_loses_crlf_and_trailing_blank_lines() {
        let w = AssWriter::new(HEADER.as_bytes(), false);
        let mut out = Vec::new();
        w.write(Vec::new(), &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\u{feff}[Script Info]\nScriptType: v4.00+\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n"
        );
    }

    #[test]
    fn dialogue_truncates_start_and_duration_separately() {
        let w = AssWriter::new(HEADER.as_bytes(), false);
        // 1.007 s for 1.499 s: mkvextract's end is 1007 + 1499 = 2506 ms -> "2.50", not 2.51.
        let (n, line) = w
            .dialogue(
                &block(1007, Some(1499), "3,0,Default,,0,0,0,,Hi, there"),
                1_000_000,
                None,
            )
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            line,
            "Dialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,Hi, there\n"
        );
    }

    #[test]
    fn dialogue_without_any_duration_ends_where_it_starts() {
        let w = AssWriter::new(HEADER.as_bytes(), false);
        let (_, line) = w
            .dialogue(&block(3_723_450, None, "0,0,S,,0,0,0,,x"), 1_000_000, None)
            .unwrap();
        assert_eq!(line, "Dialogue: 0,1:02:03.45,1:02:03.45,S,,0,0,0,,x\n");
    }

    #[test]
    fn dialogue_skips_a_non_numeric_read_order() {
        let w = AssWriter::new(HEADER.as_bytes(), false);
        assert!(
            w.dialogue(&block(0, Some(1), "x,0,S,,0,0,0,,t"), 1_000_000, None)
                .is_none()
        );
    }

    #[test]
    fn lines_come_out_in_read_order() {
        let w = AssWriter::new(HEADER.as_bytes(), false);
        let mut out = Vec::new();
        w.write(vec![(2, "b\n".into()), (0, "a\n".into())], &mut out)
            .unwrap();
        assert!(String::from_utf8(out).unwrap().ends_with("Text\na\nb\n"));
    }

    #[test]
    fn missing_events_section_gets_a_default_format() {
        let w = AssWriter::new(b"[Script Info]\nTitle: x\n", false);
        assert_eq!(w.format.len(), 10);
        assert!(w.header.ends_with(
            "[Events]\nFormat: Layer, Start, End, Style, Actor, MarginL, MarginR, MarginV, Effect, Text\n"
        ));
    }

    #[test]
    fn sections_after_events_move_to_the_end() {
        let private = b"[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\n[Fonts]\nfontname: a.ttf\n";
        let w = AssWriter::new(private, false);
        let mut out = Vec::new();
        w.write(vec![(0, "Dialogue: x\n".into())], &mut out)
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.ends_with("Text\n\nDialogue: x\n\n[Fonts]\nfontname: a.ttf\n"),
            "{text:?}"
        );
    }

    #[test]
    fn ssa_marked_field() {
        let w = AssWriter::new(
            b"[Events]\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
            true,
        );
        let (_, line) = w
            .dialogue(
                &block(0, Some(10), "1,,Default,,0000,0000,0000,,t"),
                1_000_000,
                None,
            )
            .unwrap();
        assert_eq!(
            line,
            "Dialogue: Marked=0,0:00:00.00,0:00:00.01,Default,,0000,0000,0000,,t\n"
        );
    }

    #[test]
    fn srt_entry_without_duration_waits_for_the_next_block() {
        let blocks = [
            block(1000, Some(0), "first"),
            block(2500, Some(500), "second"),
        ];
        let mut out = Vec::new();
        write_srt(&blocks, 1_000_000, None, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\u{feff}1\n00:00:01,000 --> 00:00:02,500\nfirst\n\n2\n00:00:02,500 --> 00:00:03,000\nsecond\n\n"
        );
    }

    #[test]
    fn split_matches_mkvtoolnix() {
        assert_eq!(split("a,b,c", ",", 2), ["a", "b,c"]);
        assert_eq!(split("a,b,", ",", 9), ["a", "b"]);
        assert_eq!(split("", ",", 9), [""]);
    }

    /// A time as the writer prints it, `h:mm:ss.cc` (ASS) or `hh:mm:ss,mmm` (SRT), in ns.
    fn printed_ns(t: &str) -> i128 {
        let (hms, frac) = t.split_once(['.', ',']).unwrap();
        let secs = hms
            .split(':')
            .fold(0, |acc, p| acc * 60 + p.parse::<i128>().unwrap());
        let unit = if frac.len() == 2 {
            10_000_000
        } else {
            1_000_000
        };
        secs * 1_000_000_000 + frac.parse::<i128>().unwrap() * unit
    }

    /// The start and duration (ns) the writer prints for one block on its own; `None` when it
    /// writes no line for it.
    fn printed(kind: Kind, b: &Block, scale: u64, dd: Option<u64>) -> Option<(i128, i128)> {
        let (start, end) = if kind == Kind::Srt {
            let mut out = Vec::new();
            write_srt(std::slice::from_ref(b), scale, dd, &mut out).unwrap();
            let out = String::from_utf8(out).unwrap();
            let (a, z) = out.lines().nth(1)?.split_once(" --> ").unwrap();
            (printed_ns(a), printed_ns(z))
        } else {
            let (_, line) = AssWriter::new(HEADER.as_bytes(), false).dialogue(b, scale, dd)?;
            let fields: Vec<&str> = line.split(',').collect();
            (printed_ns(fields[1]), printed_ns(fields[2]))
        };
        Some((start, end - start))
    }

    #[test]
    fn written_times_are_the_times_the_writer_prints() {
        let ms = |n: i128| n * 1_000_000;
        let frame = Some(41_708_333);
        let ass = |t, d| (Kind::Ass, block(t, d, "0,0,S,,0,0,0,,a"), 1_000_000);
        let srt = |t, d, text| (Kind::Srt, block(t, d, text), 1_000_000);
        let cases = [
            (ass(1000, Some(1500)), None, Some((ms(1000), ms(1500)))),
            // Start and end cut to the centisecond each: 1007 ms, and 2506 ms to 2.50.
            (ass(1007, Some(1499)), None, Some((ms(1000), ms(1500)))),
            // 41.708333 ms cut to 41, then to 0.04; 40 ms as it is.
            (ass(0, None), frame, Some((0, ms(40)))),
            (ass(0, None), Some(40_000_000), Some((0, ms(40)))),
            // No duration at all: a line that ends where it starts.
            (ass(3000, None), None, Some((ms(3000), 0))),
            // A ReadOrder that is no integer: mkvextract leaves the line out.
            (
                (Kind::Ass, block(0, Some(1), "x,0,S,,0,0,0,,a"), 1_000_000),
                None,
                None,
            ),
            (srt(1000, Some(1500), "a"), None, Some((ms(1000), ms(1500)))),
            (srt(0, None, "a"), Some(2_000_000_000), Some((0, ms(2000)))),
            (srt(0, None, "a"), frame, Some((0, ms(41)))),
            // No length: held for the next block. No text: never written.
            (srt(1000, Some(0), "a"), None, None),
            (srt(1000, Some(500), ""), None, None),
            // A TimestampScale of 100 µs: 1000.4 ms cut to 1000, its end 1500.4 to 1500.
            (
                (Kind::Srt, block(10_004, Some(5000), "a"), 100_000),
                None,
                Some((ms(1000), ms(500))),
            ),
        ];
        for ((kind, b, scale), dd, want) in &cases {
            assert_eq!(
                written_times(*kind, b, *scale, *dd),
                *want,
                "{kind:?} {b:?}"
            );
            assert_eq!(printed(*kind, b, *scale, *dd), *want, "{kind:?} {b:?}");
        }
        // A time before 0, which no line can hold.
        let early = block(-50, Some(1000), "0,0,S,,0,0,0,,a");
        assert_eq!(written_times(Kind::Ass, &early, 1_000_000, None), None);
    }
}
