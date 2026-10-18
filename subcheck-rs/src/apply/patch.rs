//! What an accepted fix changes: the events each finding was made from, found in a track's blocks,
//! and their new text. Pure: blocks and the run in, edits and outcomes out.

use std::collections::{BTreeMap, HashMap};

use crate::blocks::Block;
use crate::store::{Finding, Run, State};
use crate::subs::{self, Kind};
use crate::text::{self, Event};

/// A finding and the text it should read.
#[derive(Debug, Clone, PartialEq)]
pub struct Fix<'a> {
    pub finding: &'a Finding,
    pub text: String,
}

/// The run's accepted findings, in run order, each with the reviewer's wording where they reworded
/// it and else the suggestion, whitespace collapsed as the original was.
pub fn fixes(run: &Run) -> Vec<Fix<'_>> {
    run.findings
        .iter()
        .filter(|f| run.state_of(f.id) == State::Accepted)
        .map(|f| {
            let edited = run.decisions.get(&f.id).and_then(|d| d.edited.as_deref());
            Fix {
                finding: f,
                text: text::collapse(edited.unwrap_or(&f.suggestion)),
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Its events get the new text.
    Patched,
    /// Its events read the fix already.
    AlreadyApplied,
    /// Left for a fix by hand, and why.
    ByHand(String),
    /// No event reads the original or the fix: the file changed since the scan.
    NotFound,
}

/// One text track's blocks, as read.
#[derive(Debug, Clone, Copy)]
pub struct Track<'a> {
    /// mkvmerge ID.
    pub id: usize,
    pub kind: Kind,
    pub blocks: &'a [Block],
    /// Its CodecPrivate, decoded: the ASS header, written out again with the events.
    pub header: &'a [u8],
    /// TimestampScale.
    pub scale: u64,
    /// Nanoseconds.
    pub default_duration: Option<u64>,
}

/// What applying a file's fixes comes to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// One per fix, in the order given, by finding ID.
    pub outcomes: Vec<(u64, Outcome)>,
    /// New payloads, by track ID and then block index.
    pub edits: BTreeMap<usize, BTreeMap<usize, Vec<u8>>>,
}

/// `blocks` with the payloads in `edits`, by block index, in place of theirs.
pub fn edited(blocks: &[Block], edits: &BTreeMap<usize, Vec<u8>>) -> Vec<Block> {
    blocks
        .iter()
        .enumerate()
        .map(|(i, b)| match edits.get(&i) {
            Some(payload) => Block {
                payload: payload.clone(),
                ..b.clone()
            },
            None => b.clone(),
        })
        .collect()
}

/// Find the events of each fix among `tracks`, and their new payloads.
pub fn plan(tracks: &[Track], fixes: &[Fix]) -> Plan {
    let events: HashMap<usize, Vec<Option<Event>>> = tracks
        .iter()
        .map(|t| {
            let events = t
                .blocks
                .iter()
                .map(|b| text::event(t.id, t.kind, b, t.scale, t.default_duration))
                .collect();
            (t.id, events)
        })
        .collect();
    // A rewrite reads the header and every event as UTF-8 (as extract does): bytes that are not
    // would come out replaced, in lines nobody fixed.
    let utf8 = |b: &[u8]| std::str::from_utf8(subs::c_bytes(b)).is_ok();
    let rewritable: HashMap<usize, bool> = tracks
        .iter()
        .map(|t| {
            (
                t.id,
                utf8(t.header) && t.blocks.iter().all(|b| utf8(&b.payload)),
            )
        })
        .collect();
    let mut outcomes = Vec::new();
    let mut per_fix: Vec<Vec<(usize, usize, Vec<u8>)>> = Vec::new();
    for fix in fixes {
        let (outcome, edits) = match one(tracks, &events, &rewritable, fix) {
            Ok(edits) if edits.is_empty() => (Outcome::AlreadyApplied, edits),
            Ok(edits) => (Outcome::Patched, edits),
            Err(outcome) => (outcome, Vec::new()),
        };
        outcomes.push((fix.finding.id, outcome));
        per_fix.push(edits);
    }
    // Two fixes of one event would each be computed against the original: neither is made.
    let mut owners: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
    for (i, edits) in per_fix.iter().enumerate() {
        for &(track, block, _) in edits {
            owners.entry((track, block)).or_default().push(i);
        }
    }
    for fixes_of_one in owners.values().filter(|o| o.len() > 1) {
        for &i in fixes_of_one {
            outcomes[i].1 = Outcome::ByHand("two fixes on one line".into());
            per_fix[i].clear();
        }
    }
    let mut out = Plan {
        outcomes,
        edits: BTreeMap::new(),
    };
    for (track, block, payload) in per_fix.into_iter().flatten() {
        out.edits.entry(track).or_default().insert(block, payload);
    }
    out
}

/// The edits one fix makes, as (track ID, block index, payload): none when it is in already.
fn one(
    tracks: &[Track],
    events: &HashMap<usize, Vec<Option<Event>>>,
    rewritable: &HashMap<usize, bool>,
    fix: &Fix,
) -> Result<Vec<(usize, usize, Vec<u8>)>, Outcome> {
    let f = fix.finding;
    let mut edits = Vec::new();
    for o in &f.occurrences {
        let (Some(track), Some(events)) = (
            tracks.iter().find(|t| t.id == o.track),
            events.get(&o.track),
        ) else {
            return Err(Outcome::NotFound);
        };
        let markup: &[char] = match track.kind {
            Kind::Ass | Kind::Ssa => &['{', '}', '\\'],
            Kind::Srt => &['<', '>'],
        };
        if fix.text.contains(markup) {
            return Err(Outcome::ByHand("the fix has markup characters".into()));
        }
        // The events at this time, in this style: the line is among them.
        let here: Vec<(usize, &Event)> = events
            .iter()
            .enumerate()
            .filter_map(|(i, e)| Some((i, e.as_ref()?)))
            .filter(|(_, e)| e.start_ms == o.start_ms && e.end_ms == o.end_ms && e.style == f.style)
            .collect();
        let original: Vec<usize> = here
            .iter()
            .filter(|(_, e)| e.text == f.original)
            .map(|&(i, _)| i)
            .collect();
        if original.is_empty() {
            if here.iter().any(|(_, e)| e.text == fix.text) {
                continue;
            }
            return Err(match karaoke(&here, &f.original, &fix.text) {
                Some(true) => Outcome::ByHand("per-letter karaoke".into()),
                Some(false) => continue,
                None => Outcome::NotFound,
            });
        }
        if !rewritable[&track.id] {
            return Err(Outcome::ByHand("the track is not all UTF-8".into()));
        }
        for i in original {
            let block = &track.blocks[i];
            let payload = subs::c_string(&block.payload);
            let at = text::text_field(track.kind, &payload).expect("an event has a text field");
            if per_letter(&payload[at..], track.kind) {
                return Err(Outcome::ByHand("per-letter typesetting".into()));
            }
            let raw = patch_text(&payload[at..], track.kind, &f.original, &fix.text)
                .map_err(Outcome::ByHand)?;
            // Up to the first NUL, as the track is written: anything after one is not kept.
            let mut new = payload.as_bytes()[..at].to_vec();
            new.extend_from_slice(raw.as_bytes());
            edits.push((o.track, i, new));
        }
    }
    Ok(edits)
}

/// Whether the line is per-letter karaoke reading `original` (`Some(true)`) or `fixed` already
/// (`Some(false)`); `None` when no group of these events reads either.
fn karaoke(here: &[(usize, &Event)], original: &str, fixed: &str) -> Option<bool> {
    let mut rows: BTreeMap<(&str, Option<i64>), Vec<&Event>> = BTreeMap::new();
    for (_, e) in here {
        rows.entry((e.layer.as_str(), e.pos.map(|(_, y)| y)))
            .or_default()
            .push(e);
    }
    let lines: Vec<String> = rows
        .values()
        .filter_map(|g| text::karaoke_line(g))
        .collect();
    if lines.iter().any(|l| l == original) {
        Some(true)
    } else if lines.iter().any(|l| l == fixed) {
        Some(false)
    } else {
        None
    }
}

/// Whether most letters of an event have override tags of their own: a sign set along a curve,
/// each letter rotated and spaced for where it sits. A letter added or removed would move the ones
/// after it out of their places.
fn per_letter(raw: &str, kind: Kind) -> bool {
    if kind == Kind::Srt {
        return false;
    }
    let m = text::cleaned(raw, kind);
    let letters: Vec<usize> = m
        .text
        .chars()
        .enumerate()
        .filter(|(_, c)| !c.is_whitespace())
        .map(|(k, _)| m.spans[k].start)
        .collect();
    let tagged = letters
        .iter()
        .filter(|&&at| raw[..at].ends_with('}'))
        .count();
    letters.len() >= 3 && tagged * 2 >= letters.len()
}

/// `raw` (an event's text, markup and all) with the change from `original` (its cleaned text) to
/// `fixed` made in it: the characters between their common start and common end are replaced, and
/// the markup around them is kept. An error, the reason to leave it for a hand fix, when the change
/// covers markup or a line break.
pub fn patch_text(raw: &str, kind: Kind, original: &str, fixed: &str) -> Result<String, String> {
    let m = text::cleaned(raw, kind);
    debug_assert_eq!(m.text, original);
    let old: Vec<char> = original.chars().collect();
    let new: Vec<char> = fixed.chars().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    let (from, to) = (prefix, old.len() - suffix);
    let insert: String = new[prefix..new.len() - suffix].iter().collect();
    let (start, end) = if from == to {
        // An insertion goes right after the character before it, ahead of any markup that follows.
        let at = match from {
            0 => m.spans.first().map_or(0, |s| s.start),
            n => m.spans[n - 1].end,
        };
        (at, at)
    } else {
        for k in from..to {
            let source = &raw[m.spans[k].clone()];
            let plain = source == old[k].to_string()
                || (old[k] == ' ' && source.bytes().all(|b| b == b' ' || b == b'\t'));
            let joined = k + 1 == to || m.spans[k].end == m.spans[k + 1].start;
            if !plain || !joined {
                return Err("the change crosses formatting".into());
            }
        }
        (m.spans[from].start, m.spans[to - 1].end)
    };
    Ok(format!("{}{insert}{}", &raw[..start], &raw[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Occurrence;

    #[test]
    fn a_word_inside_markup_is_fixed_and_the_markup_kept() {
        let raw = r"{\i1}Ihave{\i0} to go.";
        assert_eq!(
            patch_text(raw, Kind::Ass, "Ihave to go.", "I have to go."),
            Ok(r"{\i1}I have{\i0} to go.".into())
        );
    }

    #[test]
    fn a_fix_after_a_line_break_keeps_the_break() {
        let raw = r"Two rows here\Nand teh second.";
        assert_eq!(
            patch_text(
                raw,
                Kind::Ass,
                "Two rows here and teh second.",
                "Two rows here and the second."
            ),
            Ok(r"Two rows here\Nand the second.".into())
        );
        let srt = "A line split\r\nacros two rows";
        assert_eq!(
            patch_text(
                srt,
                Kind::Srt,
                "A line split acros two rows",
                "A line split across two rows"
            ),
            Ok("A line split\r\nacross two rows".into())
        );
    }

    #[test]
    fn an_insertion_lands_before_the_markup_that_follows() {
        // "Every" then a tag: the space goes after the y, outside the bold.
        let raw = r"Every{\b1}day{\b0} again.";
        assert_eq!(
            patch_text(raw, Kind::Ass, "Everyday again.", "Every day again."),
            Ok(r"Every {\b1}day{\b0} again.".into())
        );
        // At the very start: inside the leading markup, where the first letter was.
        assert_eq!(
            patch_text(r"{\i1}ave{\i0}", Kind::Ass, "ave", "Have"),
            Ok(r"{\i1}Have{\i0}".into())
        );
    }

    #[test]
    fn a_change_across_markup_or_a_line_break_is_left_for_a_hand_fix() {
        let err = Err("the change crosses formatting".to_string());
        assert_eq!(patch_text(r"ab{\i1}cd", Kind::Ass, "abcd", "aXYd"), err);
        assert_eq!(
            patch_text(r"one\Ntwo", Kind::Ass, "one two", "one-two"),
            err
        );
        assert_eq!(
            patch_text("one\r\ntwo", Kind::Srt, "one two", "one-two"),
            err
        );
        // A run of plain spaces is text, and changes like it.
        assert_eq!(
            patch_text("one  two", Kind::Ass, "one two", "one-two"),
            Ok("one-two".into())
        );
    }

    #[test]
    fn a_repeated_letter_is_matched_from_both_ends() {
        // "than than" -> "than": the prefix and suffix must not overlap.
        assert_eq!(
            patch_text(
                "more than than ours",
                Kind::Ass,
                "more than than ours",
                "more than ours"
            ),
            Ok("more than ours".into())
        );
    }

    fn block(ms: i64, layer: u32, style: &str, text: &str) -> Block {
        Block {
            track: 1,
            position: 0,
            timestamp: ms,
            duration: Some(1000),
            payload: format!("0,{layer},{style},,0,0,0,,{text}").into_bytes(),
        }
    }

    fn finding(id: u64, style: &str, original: &str, at: &[(usize, i64)]) -> Finding {
        Finding {
            id,
            file: "a.mkv".into(),
            style: style.into(),
            original: original.into(),
            suggestion: String::new(),
            reason: String::new(),
            source: "llm".into(),
            occurrences: at
                .iter()
                .map(|&(track, start_ms)| Occurrence {
                    track,
                    start_ms,
                    end_ms: start_ms + 1000,
                })
                .collect(),
            before: vec![],
            after: vec![],
        }
    }

    fn track(id: usize, blocks: &[Block]) -> Track<'_> {
        Track {
            id,
            kind: Kind::Ass,
            blocks,
            header: b"[Script Info]",
            scale: 1_000_000,
            default_duration: None,
        }
    }

    fn payload(p: &Plan, track: usize, block: usize) -> String {
        String::from_utf8(p.edits[&track][&block].clone()).unwrap()
    }

    #[test]
    fn every_event_of_every_occurrence_is_patched() {
        // A sign on two layers in track 3, and a song line in tracks 2 and 3.
        let full = [block(9000, 0, "Song", r"{\be2}We happen share")];
        let signs = [
            block(2000, 0, "Sign", r"{\bord3}Libary"),
            block(2000, 1, "Sign", r"{\bord0}Libary"),
            block(9000, 0, "Song", r"{\be2}We happen share"),
        ];
        let sign = finding(1, "Sign", "Libary", &[(3, 2000)]);
        let song = finding(2, "Song", "We happen share", &[(2, 9000), (3, 9000)]);
        let fixes = [
            Fix {
                finding: &sign,
                text: "Library".into(),
            },
            Fix {
                finding: &song,
                text: "We happen to share".into(),
            },
        ];
        let p = plan(&[track(2, &full), track(3, &signs)], &fixes);
        assert_eq!(p.outcomes, [(1, Outcome::Patched), (2, Outcome::Patched)]);
        assert_eq!(payload(&p, 3, 0), r"0,0,Sign,,0,0,0,,{\bord3}Library");
        assert_eq!(payload(&p, 3, 1), r"0,1,Sign,,0,0,0,,{\bord0}Library");
        assert_eq!(
            payload(&p, 2, 0),
            r"0,0,Song,,0,0,0,,{\be2}We happen to share"
        );
        assert_eq!(
            payload(&p, 3, 2),
            r"0,0,Song,,0,0,0,,{\be2}We happen to share"
        );
    }

    #[test]
    fn a_fix_already_in_the_file_is_not_made_again() {
        let blocks = [block(1000, 0, "D", "We leave before it rains.")];
        let f = finding(1, "D", "We leave befor it rains.", &[(0, 1000)]);
        let fix = Fix {
            finding: &f,
            text: "We leave before it rains.".into(),
        };
        let p = plan(&[track(0, &blocks)], &[fix]);
        assert_eq!(p.outcomes, [(1, Outcome::AlreadyApplied)]);
        assert!(p.edits.is_empty());
    }

    #[test]
    fn a_line_that_is_not_there_is_not_found_and_nothing_is_patched() {
        // Found in track 0 but gone from track 1: none of it is made.
        let a = [block(1000, 0, "D", "teh end")];
        let b = [block(1000, 0, "D", "something else")];
        let f = finding(1, "D", "teh end", &[(0, 1000), (1, 1000)]);
        let fix = Fix {
            finding: &f,
            text: "the end".into(),
        };
        let p = plan(&[track(0, &a), track(1, &b)], std::slice::from_ref(&fix));
        assert_eq!(p.outcomes, [(1, Outcome::NotFound)]);
        assert!(p.edits.is_empty());
        // A different time or style is a different line.
        let f = finding(1, "Other", "teh end", &[(0, 1000)]);
        let fix = Fix {
            finding: &f,
            text: "the end".into(),
        };
        assert_eq!(
            plan(&[track(0, &a)], &[fix]).outcomes,
            [(1, Outcome::NotFound)]
        );
    }

    #[test]
    fn per_letter_karaoke_is_left_for_a_hand_fix() {
        let letters: Vec<Block> = "go trough"
            .chars()
            .enumerate()
            .map(|(i, c)| {
                let c = if c == ' ' {
                    String::new()
                } else {
                    c.to_string()
                };
                block(
                    5000,
                    0,
                    "OP",
                    &format!(r"{{\an5\pos({},40)}}{c}", 200 + 20 * i),
                )
            })
            .collect();
        let f = finding(1, "OP", "go trough", &[(0, 5000)]);
        let fix = Fix {
            finding: &f,
            text: "go through".into(),
        };
        let p = plan(&[track(0, &letters)], &[fix]);
        assert_eq!(
            p.outcomes,
            [(1, Outcome::ByHand("per-letter karaoke".into()))]
        );
        assert!(p.edits.is_empty());
    }

    #[test]
    fn a_sign_with_tags_on_every_letter_is_left_for_a_hand_fix() {
        // As typeset along a curve: each letter rotated for its place (K-On!! S00E05's title).
        let sign = r"{\org(0,900)\frz6}S{\frz5}o{\frz4}u{\frz3}v{\frz2}e{\frz1}r{\frz0}n";
        let blocks = [block(1000, 0, "Sign", sign)];
        let f = finding(1, "Sign", "Souvern", &[(0, 1000)]);
        let fix = Fix {
            finding: &f,
            text: "Souven".into(),
        };
        assert_eq!(
            plan(&[track(0, &blocks)], &[fix]).outcomes,
            [(1, Outcome::ByHand("per-letter typesetting".into()))]
        );
        // Italics around a word are not that.
        assert!(!per_letter(r"{\i1}Ihave{\i0} to go.", Kind::Ass));
    }

    #[test]
    fn two_fixes_of_one_line_are_both_left_for_a_hand_fix() {
        let blocks = [block(1000, 0, "D", "teh cat sat on teh mat")];
        let f1 = finding(1, "D", "teh cat sat on teh mat", &[(0, 1000)]);
        let f2 = finding(2, "D", "teh cat sat on teh mat", &[(0, 1000)]);
        let fixes = [
            Fix {
                finding: &f1,
                text: "the cat sat on teh mat".into(),
            },
            Fix {
                finding: &f2,
                text: "teh cat sat on the mat".into(),
            },
        ];
        let p = plan(&[track(0, &blocks)], &fixes);
        let hand = Outcome::ByHand("two fixes on one line".into());
        assert_eq!(p.outcomes, [(1, hand.clone()), (2, hand)]);
        assert!(p.edits.is_empty());
    }

    #[test]
    fn a_fix_with_markup_characters_is_left_for_a_hand_fix() {
        let blocks = [block(1000, 0, "D", "a b")];
        let f = finding(1, "D", "a b", &[(0, 1000)]);
        let fix = Fix {
            finding: &f,
            text: r"a\Nb".into(),
        };
        assert_eq!(
            plan(&[track(0, &blocks)], &[fix]).outcomes,
            [(1, Outcome::ByHand("the fix has markup characters".into()))]
        );
    }

    #[test]
    fn a_track_with_bytes_that_are_not_utf8_is_left_for_a_hand_fix() {
        // Another line of the track, in Latin-1: rewriting the track would mangle it.
        let mut cafe = block(5000, 0, "D", "caf");
        cafe.payload.push(0xE9);
        let blocks = [block(1000, 0, "D", "teh end"), cafe];
        let f = finding(1, "D", "teh end", &[(0, 1000)]);
        let fix = Fix {
            finding: &f,
            text: "the end".into(),
        };
        let p = plan(&[track(0, &blocks)], &[fix]);
        assert_eq!(
            p.outcomes,
            [(1, Outcome::ByHand("the track is not all UTF-8".into()))]
        );
        assert!(p.edits.is_empty());
    }

    #[test]
    fn a_header_that_is_not_utf8_is_left_for_a_hand_fix_but_a_fix_already_in_is_not() {
        // A Latin-1 style name in the header: a rewrite would mangle it.
        let blocks = [
            block(1000, 0, "D", "teh end"),
            block(2000, 0, "D", "the start"),
        ];
        let latin1 = [b"Style: Caf".as_slice(), &[0xE9]].concat();
        let t = Track {
            header: &latin1,
            ..track(0, &blocks)
        };
        let f1 = finding(1, "D", "teh end", &[(0, 1000)]);
        let f2 = finding(2, "D", "teh start", &[(0, 2000)]);
        let fixes = [
            Fix {
                finding: &f1,
                text: "the end".into(),
            },
            Fix {
                finding: &f2,
                text: "the start".into(),
            },
        ];
        assert_eq!(
            plan(&[t], &fixes).outcomes,
            [
                (1, Outcome::ByHand("the track is not all UTF-8".into())),
                (2, Outcome::AlreadyApplied)
            ]
        );
    }

    #[test]
    fn a_payload_is_patched_up_to_its_first_nul() {
        let mut b = block(1000, 0, "D", "teh end");
        b.payload.extend_from_slice(b"\0\0pad");
        let f = finding(1, "D", "teh end", &[(0, 1000)]);
        let fix = Fix {
            finding: &f,
            text: "the end".into(),
        };
        let p = plan(&[track(0, std::slice::from_ref(&b))], &[fix]);
        assert_eq!(p.edits[&0][&0], b"0,0,D,,0,0,0,,the end");
    }

    #[test]
    fn only_accepted_findings_are_fixes_with_the_reviewers_wording() {
        use crate::store::Decision;
        let mut run = Run::default();
        for id in 1..=3 {
            let mut f = finding(id, "D", "teh", &[(0, 1000)]);
            f.suggestion = "the".into();
            run.findings.push(f);
        }
        let decide = |finding, state, edited: Option<&str>| Decision {
            finding,
            state,
            edited: edited.map(Into::into),
            at: jiff::Timestamp::UNIX_EPOCH,
        };
        run.decisions.insert(1, decide(1, State::Accepted, None));
        run.decisions
            .insert(2, decide(2, State::Accepted, Some("  The  ")));
        run.decisions.insert(3, decide(3, State::Rejected, None));
        let got: Vec<(u64, String)> = fixes(&run)
            .into_iter()
            .map(|f| (f.finding.id, f.text))
            .collect();
        assert_eq!(got, [(1, "the".into()), (2, "The".into())]);
    }
}
