//! Subtitle events turned into lines worth proofreading: tags stripped, drawings dropped,
//! per-letter karaoke rebuilt into whole lines, duplicates merged.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use crate::blocks::Block;
use crate::subs::Kind;

/// Where one line appears: which track (mkvmerge ID) and when, in milliseconds.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct Occurrence {
    pub track: usize,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// One distinct line of text, with everywhere it appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// The ASS style; empty for SRT.
    pub style: String,
    pub text: String,
    /// Earliest first.
    pub occurrences: Vec<Occurrence>,
}

impl Line {
    pub fn start_ms(&self) -> i64 {
        self.occurrences.first().map_or(0, |o| o.start_ms)
    }
}

/// One event of one track, before karaoke grouping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub track: usize,
    pub start_ms: i64,
    pub end_ms: i64,
    pub layer: String,
    pub style: String,
    /// Text with tags stripped; empty is allowed (a karaoke space).
    pub text: String,
    /// The (x, y) of the first `\pos`, if any.
    pub pos: Option<(i64, i64)>,
    /// Has a karaoke tag (`\k`, `\K`, `\kf`, `\ko`): timed to the music, not the picture.
    pub karaoke: bool,
    /// The event as written but for its ReadOrder: Layer to Text for ASS and SSA, the text for
    /// SRT. Two tracks of one release repeat an event byte for byte.
    pub raw: String,
}

/// The track's blocks (one track, mkvmerge ID `track`) as events. `scale` is the TimestampScale;
/// `default_duration` the track's DefaultDuration in nanoseconds.
pub fn events(
    track: usize,
    kind: Kind,
    blocks: &[Block],
    scale: u64,
    default_duration: Option<u64>,
) -> Vec<Event> {
    blocks
        .iter()
        .filter_map(|b| event(track, kind, b, scale, default_duration))
        .collect()
}

/// One block as an event; `None` for an ASS block of fewer than nine fields, or a drawing.
pub fn event(
    track: usize,
    kind: Kind,
    b: &Block,
    scale: u64,
    default_duration: Option<u64>,
) -> Option<Event> {
    let start_ms = ms(i128::from(b.timestamp) * i128::from(scale));
    let duration_ms = b
        .duration
        .map(|d| ms(i128::from(d) * i128::from(scale)))
        .or(default_duration.map(|d| ms(i128::from(d))))
        .unwrap_or(0);
    // Up to the first NUL, as extract reads it: NUL padding would otherwise reach the LLM and
    // stop identical lines from matching across files.
    let payload = crate::subs::c_string(&b.payload);
    let at = text_field(kind, &payload)?;
    let (layer, style) = match kind {
        Kind::Ass | Kind::Ssa => {
            let fields: Vec<&str> = payload.splitn(9, ',').collect();
            (fields[1].to_string(), fields[2].to_string())
        }
        Kind::Srt => (String::new(), String::new()),
    };
    let Cleaned { text, pos, karaoke } = clean(&payload[at..], kind)?;
    let raw = match kind {
        Kind::Ass | Kind::Ssa => payload.split_once(',').map_or("", |(_, rest)| rest),
        Kind::Srt => &payload,
    };
    Some(Event {
        track,
        start_ms,
        end_ms: start_ms + duration_ms,
        layer,
        style,
        text,
        pos,
        karaoke,
        raw: raw.to_string(),
    })
}

/// Where the text starts in a block's payload (as `c_string` reads it): after the eighth comma
/// for ASS and SSA, whose field order Matroska fixes whatever the script's Format line says
/// (ReadOrder, Layer, Style, Name, MarginL, MarginR, MarginV, Effect, Text); the start for SRT.
/// `None` for an ASS block of fewer than nine fields.
pub fn text_field(kind: Kind, payload: &str) -> Option<usize> {
    match kind {
        Kind::Ass | Kind::Ssa => payload.match_indices(',').nth(7).map(|(i, _)| i + 1),
        Kind::Srt => Some(0),
    }
}

fn ms(ns: i128) -> i64 {
    i64::try_from(ns / 1_000_000).unwrap_or(i64::MAX)
}

/// What [`clean`] makes of an event's text.
struct Cleaned {
    text: String,
    pos: Option<(i64, i64)>,
    karaoke: bool,
}

/// Text without markup, its first `\pos`, and whether it has a karaoke tag; `None` for an ASS
/// drawing.
fn clean(raw: &str, kind: Kind) -> Option<Cleaned> {
    let mut pos = None;
    let mut karaoke = false;
    for block in tag_blocks(raw, kind) {
        if kind == Kind::Srt {
            continue;
        }
        for tag in raw[block].split('\\') {
            if is_drawing(tag) {
                return None;
            }
            if pos.is_none() {
                pos = pos_of(tag);
            }
            karaoke |= is_karaoke(tag);
        }
    }
    Some(Cleaned {
        text: cleaned(raw, kind).text,
        pos,
        karaoke,
    })
}

/// The insides of the markup blocks in `raw`, as byte ranges: `{…}` for ASS, `<…>` for SRT. An
/// opening bracket with no closing one is text.
fn tag_blocks(raw: &str, kind: Kind) -> Vec<Range<usize>> {
    let (open, close) = match kind {
        Kind::Srt => ('<', '>'),
        Kind::Ass | Kind::Ssa => ('{', '}'),
    };
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = raw[from..].find(open).map(|i| from + i) {
        let Some(j) = raw[i..].find(close).map(|j| i + j) else {
            break;
        };
        out.push(i + 1..j);
        from = j + 1;
    }
    out
}

/// A cleaned text and, for each of its characters, the bytes of the raw text it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapped {
    pub text: String,
    /// One per character of `text`.
    pub spans: Vec<Range<usize>>,
}

/// `raw` as proofread: markup stripped, line breaks (`\N`, `\n`) and hard spaces (`\h`) as spaces
/// (SRT: CR and LF), whitespace collapsed. With each character's source, so a fix to the cleaned
/// text can be made in the raw one.
pub fn cleaned(raw: &str, kind: Kind) -> Mapped {
    let mut chars: Vec<(char, Range<usize>)> = Vec::new();
    let mut from = 0;
    let push_text = |chars: &mut Vec<(char, Range<usize>)>, range: Range<usize>| {
        chars.extend(
            raw[range.clone()]
                .char_indices()
                .map(|(i, c)| (c, range.start + i..range.start + i + c.len_utf8())),
        );
    };
    for block in tag_blocks(raw, kind) {
        push_text(&mut chars, from..block.start - 1);
        from = block.end + 1;
    }
    push_text(&mut chars, from..raw.len());
    match kind {
        Kind::Srt => {
            for (c, _) in &mut chars {
                if matches!(c, '\r' | '\n') {
                    *c = ' ';
                }
            }
        }
        // One escape at a time, over the whole text, as `str::replace` would do them in turn.
        Kind::Ass | Kind::Ssa => {
            for escape in ['N', 'n', 'h'] {
                let mut out: Vec<(char, Range<usize>)> = Vec::with_capacity(chars.len());
                let mut i = 0;
                while i < chars.len() {
                    if chars[i].0 == '\\' && chars.get(i + 1).is_some_and(|n| n.0 == escape) {
                        out.push((' ', chars[i].1.start..chars[i + 1].1.end));
                        i += 2;
                    } else {
                        out.push(chars[i].clone());
                        i += 1;
                    }
                }
                chars = out;
            }
        }
    }
    // Collapse: no whitespace at the ends, and each run inside as one space spanning it.
    let mut text = String::new();
    let mut spans = Vec::new();
    let mut run: Option<Range<usize>> = None;
    for (c, span) in chars {
        if c.is_whitespace() {
            run = Some(run.map_or(span.clone(), |r| r.start..span.end));
            continue;
        }
        if let Some(r) = run.take()
            && !text.is_empty()
        {
            text.push(' ');
            spans.push(r);
        }
        text.push(c);
        spans.push(span);
    }
    Mapped { text, spans }
}

/// Whether a tag's argument is a whole number: one or more ASCII digits.
pub fn is_number(n: &str) -> bool {
    !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
}

/// `\p1` and up turn on drawing mode; `\p0` turns it off. `\pos`, `\pbo` are other tags.
fn is_drawing(tag: &str) -> bool {
    tag.strip_prefix('p')
        .filter(|n| is_number(n))
        .is_some_and(|n| !n.trim_start_matches('0').is_empty())
}

/// `\k`, `\K`, `\kf` and `\ko`, each with a duration in centiseconds.
fn is_karaoke(tag: &str) -> bool {
    tag.strip_prefix(['k', 'K'])
        .map(|rest| rest.strip_prefix(['f', 'o']).unwrap_or(rest))
        .is_some_and(is_number)
}

/// The first `\pos` in `raw` (an event's text): where its x is, as bytes of `raw`, and its value.
/// `None` for SRT, or when the first `\pos` has no x that parses.
pub fn pos_x(raw: &str, kind: Kind) -> Option<(Range<usize>, f64)> {
    if kind == Kind::Srt {
        return None;
    }
    let block = tag_blocks(raw, kind)
        .into_iter()
        .find(|b| raw[b.clone()].contains("\\pos("))?;
    let start = block.start + raw[block.clone()].find("\\pos(")? + "\\pos(".len();
    let end = start + raw[start..block.end].find(',')?;
    let x = raw[start..end].trim().parse().ok()?;
    Some((start..end, x))
}

/// Every override tag in `raw` (an event's text), in order, each without its backslash: a `\t`'s
/// inner tags come apart from it, and text before a block's first backslash (a comment) is none.
/// Empty for SRT.
pub fn tags(raw: &str, kind: Kind) -> Vec<&str> {
    if kind == Kind::Srt {
        return Vec::new();
    }
    tag_blocks(raw, kind)
        .into_iter()
        .flat_map(|b| raw[b].split('\\').skip(1))
        .filter(|tag| !tag.is_empty())
        .collect()
}

/// The tags in `raw` (an event's text) that time its effects: `\t` with its times, `\k` and its
/// kin, `\fad` and `\fade`, each as written (a `\t`'s inner tags left out). Empty for SRT.
pub fn timing(raw: &str, kind: Kind) -> Vec<&str> {
    tags(raw, kind)
        .into_iter()
        .filter(|tag| tag.starts_with("t(") || tag.starts_with("fad") || is_karaoke(tag))
        .collect()
}

fn pos_of(tag: &str) -> Option<(i64, i64)> {
    let args = tag.strip_prefix("pos(")?.trim_end_matches(')');
    let mut xy = args.split(',').map(|v| v.trim().parse::<f64>().ok());
    // \pos takes decimals; whole pixels are enough to order letters and tell rows apart.
    #[allow(clippy::cast_possible_truncation)]
    let px = |v: f64| v.round() as i64;
    let (x, y) = (xy.next()??, xy.next()??);
    Some((px(x), px(y)))
}

/// Whitespace runs as one space, none at the ends.
pub fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A style with at least this many lines, nearly all single tokens, is karaoke-effect debris.
const FRAGMENT_STYLE_MIN_LINES: usize = 10;

/// Events from every chosen track of one file, as distinct lines in time order: karaoke rebuilt,
/// lines without an ASCII letter dropped, identical (style, text) pairs merged, and fragment
/// styles dropped.
///
/// Per-syllable karaoke effects (romaji and kanji tracks especially) stagger their events in time,
/// so no grouping can rebuild them and they come out as "ya", "ba", "to". A style whose lines are
/// at least 90% single tokens, over at least [`FRAGMENT_STYLE_MIN_LINES`] occurrences, is that debris.
/// Measured on K-On!! E02: the four romaji/kanji styles are 100% single tokens, dialogue 11%, the
/// English lyrics 0–6%; a one-line sign style survives on the minimum.
pub fn lines(events: Vec<Event>) -> Vec<Line> {
    // Karaoke: events of one track in one row (see `row_of`), so two lyric rows, or signs at
    // different heights, shown at once stay separate lines.
    type Key = (usize, i64, i64, String, String, Option<i64>);
    let mut groups: BTreeMap<Key, Vec<Event>> = BTreeMap::new();
    for e in events {
        let (start, end, style, layer, y) = row_of(&e);
        let key = (e.track, start, end, style.to_string(), layer.to_string(), y);
        groups.entry(key).or_default().push(e);
    }
    let mut merged: Vec<(String, String, Occurrence)> = Vec::new();
    for ((track, start_ms, end_ms, style, _, _), group) in groups {
        let occurrence = Occurrence {
            track,
            start_ms,
            end_ms,
        };
        if let Some(joined) = karaoke_line(&group.iter().collect::<Vec<_>>()) {
            merged.push((style, joined, occurrence));
        } else {
            for e in group {
                merged.push((style.clone(), e.text, occurrence));
            }
        }
    }
    let mut by_text: HashMap<(String, String), Vec<Occurrence>> = HashMap::new();
    for (style, text, occurrence) in merged {
        if text.bytes().any(|b| b.is_ascii_alphabetic()) {
            by_text.entry((style, text)).or_default().push(occurrence);
        }
    }
    let mut out: Vec<Line> = by_text
        .into_iter()
        .map(|((style, text), mut occurrences)| {
            occurrences.sort_by_key(|o| (o.start_ms, o.track, o.end_ms));
            occurrences.dedup();
            Line {
                style,
                text,
                occurrences,
            }
        })
        .collect();
    // Counted per occurrence, not per distinct line: debris repeats ("ya" appears dozens of
    // times), so distinct lines would undercount it.
    let mut per_style: HashMap<&str, (usize, usize)> = HashMap::new();
    for l in &out {
        let (total, single) = per_style.entry(l.style.as_str()).or_default();
        *total += l.occurrences.len();
        if !l.text.contains(' ') {
            *single += l.occurrences.len();
        }
    }
    let fragments: Vec<String> = per_style
        .into_iter()
        .filter(|&(_, (total, single))| {
            total >= FRAGMENT_STYLE_MIN_LINES && single * 10 >= total * 9
        })
        .map(|(style, _)| style.to_string())
        .collect();
    out.retain(|l| !fragments.contains(&l.style));
    out.sort_by(|a, b| (a.start_ms(), &a.style, &a.text).cmp(&(b.start_ms(), &b.style, &b.text)));
    out
}

/// A row of per-letter karaoke: start and end, style, layer, and the y of the `\pos`.
pub type RowKey<'e> = (i64, i64, &'e str, &'e str, Option<i64>);

/// Which row of per-letter karaoke an event belongs to. Apply's karaoke fixes group by the same key
/// as the lines read.
pub fn row_of(e: &Event) -> RowKey<'_> {
    (
        e.start_ms,
        e.end_ms,
        e.style.as_str(),
        e.layer.as_str(),
        e.pos.map(|(_, y)| y),
    )
}

/// The line of a per-letter karaoke group: three or more events of one track that share times,
/// style, layer and row, each placed with `\pos`, joined in x order (an empty event is a space).
/// `None` for a group that is not one.
pub fn karaoke_line(group: &[&Event]) -> Option<String> {
    if group.len() < 3 || group.iter().any(|e| e.pos.is_none()) {
        return None;
    }
    let mut letters = group.to_vec();
    letters.sort_by_key(|e| e.pos.map(|(x, _)| x));
    let joined: String = letters
        .iter()
        .map(|e| {
            if e.text.is_empty() {
                " "
            } else {
                e.text.as_str()
            }
        })
        .collect();
    Some(collapse(&joined))
}

/// `H:MM:SS.mmm`: a time to the millisecond, as mpv's `--start` and `seek` take it. A frame's time
/// needs it: mpv shows the frame nearest the time it is given, and at 60 fps a centisecond is more
/// than half a frame.
pub fn timestamp_ms(ms: i64) -> String {
    let ms = ms.max(0);
    format!(
        "{}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

/// `H:MM:SS.cc`, the way ASS writes times.
pub fn timestamp(ms: i64) -> String {
    let ms = ms.max(0);
    format!(
        "{}:{:02}:{:02}.{:02}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000 / 10
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ass(read_order: u32, ms: i64, dur: u64, style: &str, text: &str) -> Block {
        Block {
            track: 1,
            position: 0,
            timestamp: ms,
            duration: Some(dur),
            payload: format!("{read_order},0,{style},,0,0,0,,{text}").into_bytes(),
        }
    }

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn tags_line_breaks_and_hard_spaces_go() {
        let ev = events(
            0,
            Kind::Ass,
            &[ass(0, 0, 1000, "D", r"{\i1}Hello{\i0}\Nthere\hyou")],
            1_000_000,
            None,
        );
        assert_eq!(texts(&lines(ev)), ["Hello there you"]);
    }

    #[test]
    fn drawings_are_dropped_but_pos_is_not_a_drawing() {
        let blocks = [
            ass(0, 0, 1000, "D", r"{\p1}m 0 0 l 10 0 10 10{\p0}"),
            ass(1, 0, 1000, "D", r"{\pos(10,20)\pbo0}Kept"),
        ];
        assert_eq!(
            texts(&lines(events(0, Kind::Ass, &blocks, 1_000_000, None))),
            ["Kept"]
        );
    }

    #[test]
    fn per_letter_karaoke_is_rebuilt_in_x_order() {
        // "Hi you" as one event per letter, written out of order, with an empty event as the space.
        let letter = |n, x: i32, t: &str| {
            ass(
                n,
                5000,
                2000,
                "OP en",
                &format!(r"{{\an5\pos({x},1013)}}{t}"),
            )
        };
        let blocks = [
            letter(0, 300, "y"),
            letter(1, 100, "H"),
            letter(2, 120, "i"),
            letter(3, 140, ""),
            letter(4, 320, "o"),
            letter(5, 340, "u"),
        ];
        let out = lines(events(0, Kind::Ass, &blocks, 1_000_000, None));
        assert_eq!(texts(&out), ["Hi you"]);
        assert_eq!(
            out[0].occurrences,
            [Occurrence {
                track: 0,
                start_ms: 5000,
                end_ms: 7000
            }]
        );
    }

    #[test]
    fn karaoke_rows_at_different_heights_are_rebuilt_separately() {
        // Two lyric rows shown together, letters interleaved in x: "ab!" at y 20, "cd?" at y 300.
        let at =
            |n, x: i32, y: i32, t: &str| ass(n, 0, 1000, "Song", &format!(r"{{\pos({x},{y})}}{t}"));
        let blocks = [
            at(0, 10, 20, "a"),
            at(1, 15, 300, "c"),
            at(2, 20, 20, "b"),
            at(3, 25, 300, "d"),
            at(4, 30, 20, "!"),
            at(5, 35, 300, "?"),
        ];
        let out = lines(events(0, Kind::Ass, &blocks, 1_000_000, None));
        assert_eq!(texts(&out), ["ab!", "cd?"]);
    }

    #[test]
    fn a_nul_padded_payload_ends_at_its_first_nul() {
        let block = Block {
            track: 1,
            position: 0,
            timestamp: 0,
            duration: Some(1000),
            payload: b"0,0,D,,0,0,0,,Hi there\0\0\0".to_vec(),
        };
        let out = lines(events(0, Kind::Ass, &[block], 1_000_000, None));
        assert_eq!(texts(&out), ["Hi there"]);
    }

    #[test]
    fn two_lines_at_the_same_time_without_pos_stay_apart() {
        let blocks = [ass(0, 0, 1000, "D", "One."), ass(1, 0, 1000, "D", "Two.")];
        assert_eq!(
            texts(&lines(events(0, Kind::Ass, &blocks, 1_000_000, None))),
            ["One.", "Two."]
        );
    }

    #[test]
    fn lines_without_ascii_letters_are_dropped() {
        let blocks = [
            ass(0, 0, 1000, "OP kanji", "空を見上げて"),
            ass(1, 0, 1000, "D", "..."),
        ];
        assert_eq!(
            lines(events(0, Kind::Ass, &blocks, 1_000_000, None)),
            [] as [Line; 0]
        );
    }

    #[test]
    fn duplicates_across_tracks_and_times_merge_into_one_line() {
        let mut ev = events(
            0,
            Kind::Ass,
            &[ass(0, 1000, 500, "D", "Same.")],
            1_000_000,
            None,
        );
        ev.extend(events(
            1,
            Kind::Ass,
            &[ass(0, 1000, 500, "D", "Same.")],
            1_000_000,
            None,
        ));
        ev.extend(events(
            0,
            Kind::Ass,
            &[ass(1, 9000, 500, "D", "Same.")],
            1_000_000,
            None,
        ));
        let out = lines(ev);
        assert_eq!(out.len(), 1);
        let tracks_and_starts: Vec<(usize, i64)> = out[0]
            .occurrences
            .iter()
            .map(|o| (o.track, o.start_ms))
            .collect();
        assert_eq!(tracks_and_starts, [(0, 1000), (1, 1000), (0, 9000)]);
    }

    #[test]
    fn a_style_of_single_token_fragments_is_dropped_but_a_small_sign_survives() {
        let mut blocks: Vec<Block> = (0..12)
            .map(|i| {
                ass(
                    i,
                    i64::from(i) * 100,
                    50,
                    "OP ro",
                    ["ya", "ba", "to"][i as usize % 3],
                )
            })
            .collect();
        blocks.push(ass(20, 5000, 1000, "Sign", "Clean-up!"));
        blocks.push(ass(21, 6000, 1000, "Dialogue", "Liar."));
        blocks.push(ass(
            22,
            7000,
            1000,
            "Dialogue",
            "You still haven't returned them?",
        ));
        let out = lines(events(0, Kind::Ass, &blocks, 1_000_000, None));
        assert_eq!(
            texts(&out),
            ["Clean-up!", "Liar.", "You still haven't returned them?"]
        );
    }

    #[test]
    fn srt_loses_html_tags_and_newlines() {
        let block = Block {
            track: 1,
            position: 0,
            timestamp: 0,
            duration: Some(1000),
            payload: b"<i>Two</i>\nrows".to_vec(),
        };
        let out = lines(events(3, Kind::Srt, &[block], 1_000_000, None));
        assert_eq!(texts(&out), ["Two rows"]);
        assert_eq!(out[0].style, "");
    }

    #[test]
    fn the_first_pos_x_is_found_where_it_is_written() {
        let raw = r"{\an5\bord3}{\pos(315.5, 1013)\pos(1,2)}T";
        let (at, x) = pos_x(raw, Kind::Ass).unwrap();
        assert_eq!((&raw[at], x), ("315.5", 315.5));
        assert_eq!(pos_x(r"{\an5}T", Kind::Ass), None);
        assert_eq!(pos_x(r"{\pos(left,2)}T", Kind::Ass), None);
        assert_eq!(pos_x(r"<\pos(1,2)>T", Kind::Srt), None);
    }

    #[test]
    fn tags_are_every_override_in_order() {
        let raw = r"{\an5\t(0,200,\alpha&H00&)}x{\pos(1,2)}";
        assert_eq!(
            tags(raw, Kind::Ass),
            ["an5", "t(0,200,", "alpha&H00&)", "pos(1,2)"]
        );
        assert_eq!(tags("<i>x</i>", Kind::Srt), Vec::<&str>::new());
        // Text before a block's first backslash is a comment, not a tag.
        assert_eq!(tags(r"{rough\an5}x", Kind::Ass), ["an5"]);
    }

    #[test]
    fn timing_tags_are_the_ones_that_time_effects() {
        let raw = r"{\an5\t(0,200,\alpha&H00&)\t(\c&H3557FF&)\k20\fad(100,0)\pos(1,2)}x";
        assert_eq!(
            timing(raw, Kind::Ass),
            ["t(0,200,", "t(", "k20", "fad(100,0)"]
        );
    }

    #[test]
    fn karaoke_tags_mark_an_event() {
        let blocks = [
            ass(0, 0, 1000, "OP", r"{\k20}ka{\k15}ra"),
            ass(1, 0, 1000, "OP", r"{\K20}ka"),
            ass(2, 0, 1000, "OP", r"{\an8\kf30}ka"),
            ass(3, 0, 1000, "OP", r"{\ko10}ka"),
            ass(4, 0, 1000, "D", r"{\pos(10,20)\i1}Not karaoke."),
            ass(5, 0, 1000, "D", "Plain."),
        ];
        let karaoke: Vec<bool> = events(0, Kind::Ass, &blocks, 1_000_000, None)
            .iter()
            .map(|e| e.karaoke)
            .collect();
        assert_eq!(karaoke, [true, true, true, true, false, false]);
    }

    /// The raw text behind each character of `m`, joined by `|`.
    fn sources(raw: &str, m: &Mapped) -> String {
        m.spans
            .iter()
            .map(|s| &raw[s.clone()])
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn each_cleaned_character_knows_its_raw_bytes() {
        let raw = r"{\i1}Ihave{\i0} to go.";
        let m = cleaned(raw, Kind::Ass);
        assert_eq!(m.text, "Ihave to go.");
        assert_eq!(sources(raw, &m), "I|h|a|v|e| |t|o| |g|o|.");
        assert_eq!(m.spans[0], 5..6, "after the {{\\i1}}");
        assert_eq!(m.spans[5], 15..16, "the space after the {{\\i0}}");
    }

    #[test]
    fn a_line_break_or_a_run_of_spaces_is_one_space_spanning_all_of_it() {
        let raw = r"Two rows\Nand  teh";
        let m = cleaned(raw, Kind::Ass);
        assert_eq!(m.text, "Two rows and teh");
        assert_eq!(sources(raw, &m), r"T|w|o| |r|o|w|s|\N|a|n|d|  |t|e|h");
        let srt = "A line split\r\nacros";
        let m = cleaned(srt, Kind::Srt);
        assert_eq!(m.text, "A line split acros");
        assert_eq!(sources(srt, &m), "A| |l|i|n|e| |s|p|l|i|t|\r\n|a|c|r|o|s");
    }

    #[test]
    fn whitespace_at_the_ends_and_markup_leave_no_characters() {
        let raw = r"  {\an8}Hi\h ";
        let m = cleaned(raw, Kind::Ass);
        assert_eq!((m.text.as_str(), m.spans.len()), ("Hi", 2));
        let html = "Hello <i>tehre</i>";
        assert_eq!(
            sources(html, &cleaned(html, Kind::Srt)),
            "H|e|l|l|o| |t|e|h|r|e"
        );
        // An opening brace that never closes is text, as it is to clean.
        assert_eq!(cleaned("a {b", Kind::Ass).text, "a {b");
    }

    #[test]
    fn the_text_field_follows_the_eighth_comma() {
        assert_eq!(text_field(Kind::Ass, "0,0,D,,0,0,0,,Hi, there"), Some(14));
        assert_eq!(text_field(Kind::Ass, "0,0,D,,0,0,0"), None);
        assert_eq!(text_field(Kind::Srt, "Hi, there"), Some(0));
    }

    #[test]
    fn a_block_is_an_event_unless_it_is_a_drawing_or_too_short() {
        let ok = event(2, Kind::Ass, &ass(0, 1500, 500, "D", "Hi"), 1_000_000, None).unwrap();
        assert_eq!(
            (ok.track, ok.start_ms, ok.end_ms, ok.text.as_str()),
            (2, 1500, 2000, "Hi")
        );
        let drawing = ass(0, 0, 1, "D", r"{\p1}m 0 0 l 1 1{\p0}");
        assert_eq!(event(0, Kind::Ass, &drawing, 1_000_000, None), None);
        let short = Block {
            payload: b"0,0,D".to_vec(),
            ..ass(0, 0, 1, "D", "")
        };
        assert_eq!(event(0, Kind::Ass, &short, 1_000_000, None), None);
    }

    #[test]
    fn an_events_raw_text_is_all_but_its_read_order() {
        let sign = ass(7, 0, 1, "Sign", r"{\pos(1,2)}Hi");
        let a = event(0, Kind::Ass, &sign, 1_000_000, None).unwrap();
        assert_eq!(a.raw, r"0,Sign,,0,0,0,,{\pos(1,2)}Hi");
        let srt = Block {
            payload: b"<i>Two</i>\nrows".to_vec(),
            ..ass(0, 0, 1, "", "")
        };
        let b = event(0, Kind::Srt, &srt, 1_000_000, None).unwrap();
        assert_eq!(b.raw, "<i>Two</i>\nrows");
    }

    #[test]
    fn timestamps_print_like_ass() {
        assert_eq!(timestamp(779_080), "0:12:59.08");
        assert_eq!(timestamp(3_723_450), "1:02:03.45");
    }
}
