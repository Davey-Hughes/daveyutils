//! Subtitle events turned into lines worth proofreading: tags stripped, drawings dropped,
//! per-letter karaoke rebuilt into whole lines, duplicates merged.

use std::collections::{BTreeMap, HashMap};

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
    let mut out = Vec::new();
    for b in blocks {
        let start_ms = ms(i128::from(b.timestamp) * i128::from(scale));
        let duration_ms = b
            .duration
            .map(|d| ms(i128::from(d) * i128::from(scale)))
            .or(default_duration.map(|d| ms(i128::from(d))))
            .unwrap_or(0);
        // Up to the first NUL, as extract reads it: NUL padding would otherwise reach the LLM and
        // stop identical lines from matching across files.
        let payload = crate::subs::c_string(&b.payload);
        let (layer, style, raw) = match kind {
            // Matroska fixes the block's field order, whatever the script's Format line says:
            // ReadOrder, Layer, Style, Name, MarginL, MarginR, MarginV, Effect, Text.
            Kind::Ass | Kind::Ssa => {
                let fields: Vec<&str> = payload.splitn(9, ',').collect();
                if fields.len() < 9 {
                    continue;
                }
                (
                    fields[1].to_string(),
                    fields[2].to_string(),
                    fields[8].to_string(),
                )
            }
            Kind::Srt => (String::new(), String::new(), payload),
        };
        let Some((text, pos)) = clean(&raw, kind) else {
            continue;
        };
        out.push(Event {
            track,
            start_ms,
            end_ms: start_ms + duration_ms,
            layer,
            style,
            text,
            pos,
        });
    }
    out
}

fn ms(ns: i128) -> i64 {
    i64::try_from(ns / 1_000_000).unwrap_or(i64::MAX)
}

/// Text without markup, and its first `\pos`; `None` for an ASS drawing.
fn clean(raw: &str, kind: Kind) -> Option<(String, Option<(i64, i64)>)> {
    let mut text = String::new();
    let mut pos = None;
    let mut rest = raw;
    while !rest.is_empty() {
        let (open, close) = match kind {
            Kind::Srt => ('<', '>'),
            Kind::Ass | Kind::Ssa => ('{', '}'),
        };
        let Some(i) = rest.find(open) else {
            text.push_str(rest);
            break;
        };
        text.push_str(&rest[..i]);
        let Some(j) = rest[i..].find(close) else {
            text.push_str(&rest[i..]);
            break;
        };
        let block = &rest[i + 1..i + j];
        if kind != Kind::Srt {
            for tag in block.split('\\') {
                if is_drawing(tag) {
                    return None;
                }
                if pos.is_none() {
                    pos = pos_of(tag);
                }
            }
        }
        rest = &rest[i + j + 1..];
    }
    let text = match kind {
        Kind::Srt => text.replace(['\r', '\n'], " "),
        Kind::Ass | Kind::Ssa => text
            .replace("\\N", " ")
            .replace("\\n", " ")
            .replace("\\h", " "),
    };
    Some((collapse(&text), pos))
}

/// `\p1` and up turn on drawing mode; `\p0` turns it off. `\pos`, `\pbo` are other tags.
fn is_drawing(tag: &str) -> bool {
    tag.strip_prefix('p')
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .is_some_and(|n| !n.trim_start_matches('0').is_empty())
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

fn collapse(s: &str) -> String {
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
    // Karaoke: events of one track sharing style, layer, start, end and row (the \pos y), so
    // two lyric rows, or signs at different heights, shown at once stay separate lines.
    type Key = (usize, i64, i64, String, String, Option<i64>);
    let mut groups: BTreeMap<Key, Vec<Event>> = BTreeMap::new();
    for e in events {
        let key = (
            e.track,
            e.start_ms,
            e.end_ms,
            e.style.clone(),
            e.layer.clone(),
            e.pos.map(|(_, y)| y),
        );
        groups.entry(key).or_default().push(e);
    }
    let mut merged: Vec<(String, String, Occurrence)> = Vec::new();
    for ((track, start_ms, end_ms, style, _, _), mut group) in groups {
        let occurrence = Occurrence {
            track,
            start_ms,
            end_ms,
        };
        if group.len() >= 3 && group.iter().all(|e| e.pos.is_some()) {
            group.sort_by_key(|e| e.pos.map(|(x, _)| x));
            let joined: String = group
                .iter()
                .map(|e| {
                    if e.text.is_empty() {
                        " "
                    } else {
                        e.text.as_str()
                    }
                })
                .collect();
            merged.push((style, collapse(&joined), occurrence));
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
    fn timestamps_print_like_ass() {
        assert_eq!(timestamp(779_080), "0:12:59.08");
        assert_eq!(timestamp(3_723_450), "1:02:03.45");
    }
}
