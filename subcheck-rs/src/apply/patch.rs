//! What an accepted fix changes: the events each finding was made from, found in a track's blocks,
//! and their new text. Pure: blocks and the run in, edits and outcomes out.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use super::karaoke::{self, Align, Letter, Widths};
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
    /// Its per-letter karaoke rows get the new letters and are re-spaced: `px` wider (its first
    /// row; narrower when negative), with the widths of the characters in `guessed` measured in no
    /// row's style.
    Respaced { px: i64, guessed: String },
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

/// What the fixes do to one track's blocks.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Edits {
    /// New payloads, by block index.
    pub changed: BTreeMap<usize, Vec<u8>>,
    /// Blocks left out, by index.
    pub removed: BTreeSet<usize>,
    /// New blocks, by the index of the block each is cloned from and takes its times from.
    pub added: BTreeMap<usize, Added>,
}

/// New blocks beside one block: right before it and right after it, each in order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Added {
    pub before: Vec<Vec<u8>>,
    pub after: Vec<Vec<u8>>,
}

/// A per-letter karaoke row re-spaced, which the dry run shows as one line rather than event by
/// event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KaraokeRow {
    pub track: usize,
    /// Its events, by block index.
    pub blocks: Vec<usize>,
    pub was: String,
    pub now: String,
    /// Letters kept that now read another character.
    pub changed: usize,
    pub added: usize,
    pub removed: usize,
    /// Letters kept that moved.
    pub moved: usize,
}

/// What applying a file's fixes comes to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// One per fix, in the order given, by finding ID.
    pub outcomes: Vec<(u64, Outcome)>,
    /// By track ID.
    pub edits: BTreeMap<usize, Edits>,
    /// The karaoke rows re-spaced, in fix order.
    pub rows: Vec<KaraokeRow>,
}

/// `blocks` as `edits` leaves them: payloads replaced, blocks left out, and new blocks beside the
/// ones they were cloned from, with their times.
pub fn edited(blocks: &[Block], edits: &Edits) -> Vec<Block> {
    let with = |b: &Block, payload: &Vec<u8>| Block {
        payload: payload.clone(),
        ..b.clone()
    };
    let mut out = Vec::with_capacity(blocks.len());
    for (i, b) in blocks.iter().enumerate() {
        let added = edits.added.get(&i);
        out.extend(
            added
                .into_iter()
                .flat_map(|a| &a.before)
                .map(|p| with(b, p)),
        );
        if !edits.removed.contains(&i) {
            out.push(
                edits
                    .changed
                    .get(&i)
                    .map_or_else(|| b.clone(), |p| with(b, p)),
            );
        }
        out.extend(added.into_iter().flat_map(|a| &a.after).map(|p| with(b, p)));
    }
    out
}

/// What one fix does.
#[derive(Debug, Default)]
struct Change {
    /// By track ID.
    edits: BTreeMap<usize, Edits>,
    /// The events it depends on, as (track ID, block index): those it edits, and every event of a
    /// karaoke row it re-spaces.
    owned: BTreeSet<(usize, usize)>,
    rows: Vec<KaraokeRow>,
    /// A karaoke fix's growth in pixels (its first row's), and every row's guessed characters.
    karaoke: Option<(i64, BTreeSet<char>)>,
}

/// What every fix of a file reads.
struct Context<'a> {
    tracks: &'a [Track<'a>],
    /// Each track's blocks as events, by track ID and block index.
    events: HashMap<usize, Vec<Option<Event>>>,
    /// Whether a track can be rewritten: its header and every event are UTF-8.
    rewritable: HashMap<usize, bool>,
    /// Re-space per-letter karaoke (`--karaoke`), rather than leave it for a hand fix.
    karaoke: bool,
    /// Each style's alignment, by track ID and style, read when first needed.
    aligns: HashMap<(usize, String), Result<Align, &'static str>>,
    /// Letter widths, by track ID, style and the width-changing tags of the letters (see
    /// [`widening`]), fitted when first needed.
    widths: HashMap<(usize, String, Vec<String>), Widths>,
}

/// Find the events of each fix among `tracks`, and their new payloads. `karaoke`: re-space
/// per-letter karaoke rows rather than leave them for a hand fix.
pub fn plan(tracks: &[Track], fixes: &[Fix], karaoke: bool) -> Plan {
    let events = tracks
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
    let rewritable = tracks
        .iter()
        .map(|t| {
            (
                t.id,
                utf8(t.header) && t.blocks.iter().all(|b| utf8(&b.payload)),
            )
        })
        .collect();
    let mut cx = Context {
        tracks,
        events,
        rewritable,
        karaoke,
        aligns: HashMap::new(),
        widths: HashMap::new(),
    };
    let mut outcomes = Vec::new();
    let mut changes: Vec<Change> = Vec::new();
    for fix in fixes {
        let (outcome, change) = match cx.one(fix) {
            Ok(c) if c.owned.is_empty() => (Outcome::AlreadyApplied, c),
            Ok(mut c) => {
                let outcome =
                    c.karaoke
                        .take()
                        .map_or(Outcome::Patched, |(px, guessed)| Outcome::Respaced {
                            px,
                            guessed: guessed.into_iter().collect(),
                        });
                (outcome, c)
            }
            Err(outcome) => (outcome, Change::default()),
        };
        outcomes.push((fix.finding.id, outcome));
        changes.push(change);
    }
    // Two fixes of one event would each be computed against the original: neither is made.
    let mut owners: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
    for (i, c) in changes.iter().enumerate() {
        for &event in &c.owned {
            owners.entry(event).or_default().push(i);
        }
    }
    for fixes_of_one in owners.values().filter(|o| o.len() > 1) {
        for &i in fixes_of_one {
            outcomes[i].1 = Outcome::ByHand("two fixes on one line".into());
            changes[i] = Change::default();
        }
    }
    let mut out = Plan {
        outcomes,
        ..Plan::default()
    };
    for c in changes {
        for (track, e) in c.edits {
            let into = out.edits.entry(track).or_default();
            into.changed.extend(e.changed);
            into.removed.extend(e.removed);
            into.added.extend(e.added);
        }
        out.rows.extend(c.rows);
    }
    out
}

impl Context<'_> {
    /// What one fix changes: nothing when it is in already.
    fn one(&mut self, fix: &Fix) -> Result<Change, Outcome> {
        let f = fix.finding;
        let mut change = Change::default();
        // Reworded back to the line itself: nothing to change, so nothing to remux.
        if fix.text == f.original {
            return Ok(change);
        }
        for o in &f.occurrences {
            let Some(track) = self.tracks.iter().find(|t| t.id == o.track) else {
                return Err(Outcome::NotFound);
            };
            let markup: &[char] = match track.kind {
                Kind::Ass | Kind::Ssa => &['{', '}', '\\'],
                Kind::Srt => &['<', '>'],
            };
            if fix.text.contains(markup) {
                return Err(Outcome::ByHand("the fix has markup characters".into()));
            }
            let events = &self.events[&track.id];
            // The events at this time, in this style: the line is among them.
            let here: Vec<(usize, &Event)> = events
                .iter()
                .enumerate()
                .filter_map(|(i, e)| Some((i, e.as_ref()?)))
                .filter(|(_, e)| {
                    e.start_ms == o.start_ms && e.end_ms == o.end_ms && e.style == f.style
                })
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
                let rows = karaoke_rows(&here);
                let of_original: Vec<&Group> = rows
                    .iter()
                    .filter(|(_, line)| *line == f.original)
                    .map(|(g, _)| g)
                    .collect();
                if of_original.is_empty() {
                    if rows.iter().any(|(_, line)| *line == fix.text) {
                        continue;
                    }
                    return Err(Outcome::NotFound);
                }
                if !self.rewritable[&track.id] {
                    return Err(Outcome::ByHand("the track is not all UTF-8".into()));
                }
                let align = *self
                    .aligns
                    .entry((track.id, f.style.clone()))
                    .or_insert_with(|| karaoke::alignment(track.header, &f.style));
                let align = align.map_err(|why| Outcome::ByHand(why.into()))?;
                for group in of_original {
                    // A letter set moving (no \pos) leaves a hole in the row of \pos letters.
                    let layer = &group[0].1.layer;
                    if here
                        .iter()
                        .any(|(_, e)| e.pos.is_none() && e.layer == *layer)
                    {
                        return Err(Outcome::ByHand(
                            "karaoke row has a letter without \\pos".into(),
                        ));
                    }
                    let (letters, size) = read_row(track, group, align)?;
                    let widths = self
                        .widths
                        .entry((track.id, f.style.clone(), size))
                        .or_insert_with_key(|(_, style, size)| {
                            fit_style(track, events, style, size, align)
                        });
                    let row = (&letters[..], f.original.as_str());
                    respace_row(track, row, &fix.text, widths, align, &mut change)?;
                }
                // Worked out in full first: the hint is given only where --karaoke would do it.
                if !self.karaoke {
                    return Err(Outcome::ByHand("per-letter karaoke (try --karaoke)".into()));
                }
                continue;
            }
            if !self.rewritable[&track.id] {
                return Err(Outcome::ByHand("the track is not all UTF-8".into()));
            }
            for i in original {
                let new = patch_event(track, i, &f.original, &fix.text)?;
                change
                    .edits
                    .entry(o.track)
                    .or_default()
                    .changed
                    .insert(i, new);
                change.owned.insert((o.track, i));
            }
        }
        Ok(change)
    }
}

/// Block `i` of `track` (an ordinary line reading `original`) with the change to `fixed` made in
/// its text; an error, the reason to leave it for a hand fix, when that cannot be done safely.
fn patch_event(track: &Track, i: usize, original: &str, fixed: &str) -> Result<Vec<u8>, Outcome> {
    let payload = subs::c_string(&track.blocks[i].payload);
    let at = text::text_field(track.kind, &payload).expect("an event has a text field");
    if per_letter(&payload[at..], track.kind) {
        return Err(Outcome::ByHand("per-letter typesetting".into()));
    }
    let raw = patch_text(&payload[at..], track.kind, original, fixed).map_err(Outcome::ByHand)?;
    // Up to the first NUL, as the track is written: anything after one is not kept.
    let mut new = payload.as_bytes()[..at].to_vec();
    new.extend_from_slice(raw.as_bytes());
    Ok(new)
}

/// Events of one karaoke row, with their block indices.
type Group<'e> = Vec<(usize, &'e Event)>;

/// The per-letter karaoke rows among `here` (events of one time and style), each with its line:
/// grouped as `text::lines` groups them (`text::row_of`).
fn karaoke_rows<'e>(here: &[(usize, &'e Event)]) -> Vec<(Group<'e>, String)> {
    let mut rows: BTreeMap<text::RowKey, Group> = BTreeMap::new();
    for &(i, e) in here {
        rows.entry(text::row_of(e)).or_default().push((i, e));
    }
    rows.into_values()
        .filter_map(|g| {
            let line = text::karaoke_line(&g.iter().map(|(_, e)| *e).collect::<Vec<_>>())?;
            Some((g, line))
        })
        .collect()
}

/// An event's one letter: its character, a space when it is empty; `None` for two or more.
fn letter_of(e: &Event) -> Option<char> {
    let mut chars = e.text.chars();
    match (chars.next(), chars.next()) {
        (None, _) => Some(' '),
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// The tags in `raw` (an event's text) that change how wide its letters are drawn: letters with the
/// same ones, in one style, are drawn at the same widths.
fn widening(raw: &str, kind: Kind) -> Vec<&str> {
    text::tags(raw, kind)
        .into_iter()
        .filter(|t| karaoke::widens(t))
        .collect()
}

/// Whether a letter sits on its `\pos` by its centre: its own first alignment tag (libass heeds a
/// line's first only), else `align`, its style's.
fn centred(raw: &str, kind: Kind, align: Align) -> bool {
    let own = text::tags(raw, kind)
        .into_iter()
        .find_map(karaoke::tag_alignment);
    own.flatten().unwrap_or(align) == Align::Centre
}

/// Letter widths for `style` in `track`, from every per-letter row of it with its letters centred
/// on their `\pos` (`align` is the style's) and drawn at `size` (their [`widening`] tags; a blank
/// draws nothing, so its own do not count): three or more events of one row (`text::row_of`), one
/// letter each. A row beside an event with no `\pos` (a letter set moving with `\move`, say) is
/// left out: the gap it leaves would read as a pair of neighbours.
fn fit_style(
    track: &Track,
    events: &[Option<Event>],
    style: &str,
    size: &[String],
    align: Align,
) -> Widths {
    let mut rows: BTreeMap<text::RowKey, Option<karaoke::Row>> = BTreeMap::new();
    let mut gaps: BTreeSet<text::RowKey> = BTreeSet::new();
    for (i, e) in events.iter().enumerate() {
        let Some(e) = e.as_ref().filter(|e| e.style == style) else {
            continue;
        };
        let key = text::row_of(e);
        if key.4.is_none() {
            gaps.insert(key);
            continue;
        }
        let payload = subs::c_string(&track.blocks[i].payload);
        let raw = text::text_field(track.kind, &payload).map(|at| &payload[at..]);
        let x = raw.and_then(|r| text::pos_x(r, track.kind)).map(|(_, x)| x);
        let letter = letter_of(e);
        let usable = raw.is_some_and(|r| {
            centred(r, track.kind, align)
                && (letter == Some(' ') || widening(r, track.kind) == size)
        });
        let row = rows.entry(key).or_insert_with(|| Some(Vec::new()));
        // A syllable, a \pos that does not parse, or a letter not centred or drawn at another size:
        // not a row to learn from.
        match (row.as_mut(), x, letter, usable) {
            (Some(row), Some(x), Some(c), true) => row.push((x, c)),
            _ => *row = None,
        }
    }
    let rows: Vec<karaoke::Row> = rows
        .into_iter()
        .filter(|((start, end, style, layer, _), _)| {
            !gaps.contains(&(*start, *end, *style, *layer, None))
        })
        .filter_map(|(_, row)| row)
        .filter(|r| r.len() >= 3)
        .map(|mut r| {
            r.sort_by(|a, b| a.0.total_cmp(&b.0));
            r
        })
        .collect();
    Widths::fit(&rows)
}

/// One letter of a karaoke row as read: its block, payload, where its text starts, where its x
/// is in that text, and the x and character.
struct Read {
    block: usize,
    payload: String,
    at: usize,
    x_at: Range<usize>,
    x: f64,
    c: char,
}

impl Read {
    /// Its text field, markup and all.
    fn raw(&self) -> &str {
        &self.payload[self.at..]
    }

    /// Its x as written.
    fn x_text(&self) -> &str {
        &self.raw()[self.x_at.clone()]
    }

    /// Its fields before the text (ReadOrder and all), then the text of `from` with the x written as
    /// `x` and the letter as `c` (a space empties it). `from` is itself but where its tags would not
    /// do: a blank, which may lack the row's tags, made a letter.
    fn with(&self, from: &Read, x: &str, c: char, kind: Kind) -> String {
        let raw = from.raw();
        let m = text::cleaned(raw, kind);
        let letter = m.spans.first().cloned().unwrap_or(raw.len()..raw.len());
        let c = if c == ' ' {
            String::new()
        } else {
            c.to_string()
        };
        // The later of the two first, so the earlier one's bytes stay where they are.
        let mut edits = [(from.x_at.clone(), x.to_string()), (letter, c)];
        edits.sort_by_key(|e| std::cmp::Reverse(e.0.start));
        let mut raw = raw.to_string();
        for (range, with) in edits {
            raw.replace_range(range, &with);
        }
        format!("{}{raw}", &self.payload[..self.at])
    }
}

/// A karaoke row's letters as read, in x order, and the [`widening`] tags they carry. `align` is
/// the style's: a letter without an alignment tag of its own sits on its `\pos` by it. An error,
/// the reason to leave the row for a hand fix, when it is not one letter per event, a letter is not
/// centred on its `\pos` (the widths read the positions as centres), or the letters (a blank draws
/// nothing, so not the blanks) are drawn at different sizes or timed one by one.
fn read_row(
    track: &Track,
    group: &[(usize, &Event)],
    align: Align,
) -> Result<(Vec<Read>, Vec<String>), Outcome> {
    let hand = |why: &str| Outcome::ByHand(why.into());
    let mut letters = Vec::new();
    for &(block, e) in group {
        let payload = subs::c_string(&track.blocks[block].payload);
        let at = text::text_field(track.kind, &payload).expect("an event has a text field");
        let (x_at, x) = text::pos_x(&payload[at..], track.kind)
            .ok_or_else(|| hand("a letter's \\pos does not parse"))?;
        let c = letter_of(e).ok_or_else(|| hand("karaoke not one letter per event"))?;
        if !centred(&payload[at..], track.kind, align) {
            return Err(hand("karaoke letters not centred on their \\pos"));
        }
        let placed = ["clip(", "iclip(", "org(", "move("];
        if text::tags(&payload[at..], track.kind)
            .iter()
            .any(|t| placed.iter().any(|p| t.starts_with(p)))
        {
            return Err(hand("karaoke letters carry \\clip, \\org or \\move"));
        }
        letters.push(Read {
            block,
            payload,
            at,
            x_at,
            x,
            c,
        });
    }
    letters.sort_by(|a, b| a.x.total_cmp(&b.x));
    // Blank letters at an end, or two in a row, are not in the line: its letters would not line up.
    let joined: String = letters.iter().map(|l| l.c).collect();
    if joined != text::collapse(&joined) {
        return Err(hand("karaoke not one letter per event"));
    }
    let drawn: Vec<&Read> = letters.iter().filter(|l| l.c != ' ').collect();
    let first = drawn[0];
    let size = widening(first.raw(), track.kind);
    if drawn.iter().any(|l| widening(l.raw(), track.kind) != size) {
        return Err(hand("karaoke letters drawn at different sizes"));
    }
    let timing = text::timing(first.raw(), track.kind);
    if drawn
        .iter()
        .any(|l| text::timing(l.raw(), track.kind) != timing)
    {
        return Err(hand("per-letter timing"));
    }
    let size = size.into_iter().map(str::to_string).collect();
    Ok((letters, size))
}

/// How a karaoke row writes its positions: with as many decimals as its most precise x.
struct Precision {
    decimals: usize,
    scale: f64,
}

impl Precision {
    fn of(letters: &[Read]) -> Precision {
        let decimals = letters
            .iter()
            .map(|l| {
                l.x_text()
                    .trim()
                    .split_once('.')
                    .map_or(0, |(_, d)| d.len())
            })
            .max()
            .unwrap_or(0);
        Precision {
            decimals,
            scale: (0..decimals).fold(1.0, |s, _| s * 10.0),
        }
    }

    /// `v` rounded to the row's precision, halves away from zero.
    fn round(&self, v: f64) -> f64 {
        (v * self.scale).round() / self.scale
    }

    fn write(&self, x: f64) -> String {
        format!("{:.*}", self.decimals, self.round(x))
    }
}

/// The edits that change one karaoke row to `fixed`, re-spaced. `row` is its letters as
/// [`read_row`] reads them, and the line they were found reading.
fn respace_row(
    track: &Track,
    (letters, original): (&[Read], &str),
    fixed: &str,
    widths: &Widths,
    align: Align,
    change: &mut Change,
) -> Result<(), Outcome> {
    let row: karaoke::Row = letters.iter().map(|l| (l.x, l.c)).collect();
    let old: Vec<char> = row.iter().map(|l| l.1).collect();
    // The line was read in whole-pixel order: two letters on one pixel may read otherwise here.
    if old.iter().collect::<String>() != original {
        return Err(Outcome::ByHand("karaoke letters overlap".into()));
    }
    if !widths.fits(&row) {
        return Err(Outcome::ByHand("karaoke letter widths do not fit".into()));
    }
    let new: Vec<char> = fixed.chars().collect();
    let (from, to, new_to) = differ(&old, &new);
    let precision = Precision::of(letters);
    let span = &new[from..new_to];
    let respaced = karaoke::respace(&row, from, to, span, widths, align, |v| precision.round(v))
        .ok_or_else(|| Outcome::ByHand("the change covers the whole line".into()))?;
    // A letter drawn like the row's: the nearest one that is not a blank, before `k` if any.
    let drawn_near = |k: usize| {
        (0..=k)
            .rev()
            .chain(k + 1..letters.len())
            .find(|&i| letters[i].c != ' ')
            .expect("a row has a letter that is not a blank")
    };
    let edits = change.edits.entry(track.id).or_default();
    let (mut changed, mut added, mut moved) = (0, 0, 0);
    let mut kept = BTreeSet::new();
    for &(letter, x, c) in &respaced.letters {
        match letter {
            Letter::Old(k) => {
                let l = &letters[k];
                // Each letter moves by its shift rounded to the row's precision: the same on both
                // sides of the change, so a centred row stays centred. One that stays put keeps its
                // x as it is written (" 315").
                let shift = precision.round(x - l.x);
                let x = if shift == 0.0 {
                    l.x_text().to_string()
                } else {
                    moved += 1;
                    precision.write(l.x + shift)
                };
                if c != l.c {
                    changed += 1;
                }
                let from = if c == ' ' || l.c != ' ' {
                    l
                } else {
                    &letters[drawn_near(k)]
                };
                let new = l.with(from, &x, c, track.kind);
                if new != l.payload {
                    edits.changed.insert(l.block, new.into_bytes());
                }
                kept.insert(k);
            }
            Letter::New { from, after } => {
                // Beside the letter it was placed by (and with its ReadOrder), drawn like a letter.
                let l = &letters[from];
                let like = if c == ' ' {
                    l
                } else {
                    &letters[drawn_near(from)]
                };
                let new = l
                    .with(like, &precision.write(x), c, track.kind)
                    .into_bytes();
                let beside = edits.added.entry(l.block).or_default();
                if after {
                    beside.after.push(new);
                } else {
                    beside.before.push(new);
                }
                added += 1;
            }
        }
    }
    let removed: Vec<usize> = (0..letters.len())
        .filter(|k| !kept.contains(k))
        .map(|k| letters[k].block)
        .collect();
    edits.removed.extend(&removed);
    change
        .owned
        .extend(letters.iter().map(|l| (track.id, l.block)));
    change.rows.push(KaraokeRow {
        track: track.id,
        blocks: letters.iter().map(|l| l.block).collect(),
        was: old.iter().collect(),
        now: fixed.to_string(),
        changed,
        added,
        removed: removed.len(),
        moved,
    });
    #[allow(clippy::cast_possible_truncation)] // a row's growth in pixels
    let px = respaced.growth.round() as i64;
    let (_, guessed) = change.karaoke.get_or_insert_with(|| (px, BTreeSet::new()));
    guessed.extend(&respaced.guessed);
    Ok(())
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

/// Where `old` and `new` differ: the end of their common start, and where their common end begins
/// in `old` and in `new`. The two never overlap.
fn differ(old: &[char], new: &[char]) -> (usize, usize, usize) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    (prefix, old.len() - suffix, new.len() - suffix)
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
    let (from, to, new_to) = differ(&old, &new);
    let insert: String = new[from..new_to].iter().collect();
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

    /// OP is centred (8, at the top), Left left-aligned (7).
    const HEADER: &[u8] =
        b"[Script Info]\n\n[V4+ Styles]\nFormat: Name, Alignment\nStyle: OP,8\nStyle: Left,7\n";

    fn track(id: usize, blocks: &[Block]) -> Track<'_> {
        Track {
            id,
            kind: Kind::Ass,
            blocks,
            header: HEADER,
            scale: 1_000_000,
            default_duration: None,
        }
    }

    fn payload(p: &Plan, track: usize, block: usize) -> String {
        String::from_utf8(p.edits[&track].changed[&block].clone()).unwrap()
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
        let p = plan(&[track(2, &full), track(3, &signs)], &fixes, false);
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
        let p = plan(&[track(0, &blocks)], &[fix], false);
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
        let p = plan(
            &[track(0, &a), track(1, &b)],
            std::slice::from_ref(&fix),
            false,
        );
        assert_eq!(p.outcomes, [(1, Outcome::NotFound)]);
        assert!(p.edits.is_empty());
        // A different time or style is a different line.
        let f = finding(1, "Other", "teh end", &[(0, 1000)]);
        let fix = Fix {
            finding: &f,
            text: "the end".into(),
        };
        assert_eq!(
            plan(&[track(0, &a)], &[fix], false).outcomes,
            [(1, Outcome::NotFound)]
        );
    }

    /// A per-letter row of `text` at `ms` on `layer`: letters 20 px apart from x 200, each
    /// `{\an5<tags>\pos(x,40)}c`, a space an empty event.
    fn row(ms: i64, layer: u32, style: &str, tags: &str, text: &str) -> Vec<Block> {
        text.chars()
            .enumerate()
            .map(|(i, c)| {
                let c = if c == ' ' {
                    String::new()
                } else {
                    c.to_string()
                };
                let x = 200 + 20 * i;
                block(ms, layer, style, &format!(r"{{\an5{tags}\pos({x},40)}}{c}"))
            })
            .collect()
    }

    /// Plan one fix of `original` to `fixed` in `style` at 5 s, on the given tracks.
    fn plan_one(tracks: &[Track], style: &str, original: &str, fixed: &str, karaoke: bool) -> Plan {
        let at: Vec<(usize, i64)> = tracks.iter().map(|t| (t.id, 5000)).collect();
        let f = finding(1, style, original, &at);
        let fix = Fix {
            finding: &f,
            text: fixed.into(),
        };
        plan(tracks, &[fix], karaoke)
    }

    fn hand(why: &str) -> Vec<(u64, Outcome)> {
        vec![(1, Outcome::ByHand(why.into()))]
    }

    #[test]
    fn per_letter_karaoke_is_left_for_a_hand_fix_without_the_flag() {
        let letters = row(5000, 0, "OP", "", "go trough");
        let p = plan_one(
            &[track(0, &letters)],
            "OP",
            "go trough",
            "go through",
            false,
        );
        assert_eq!(p.outcomes, hand("per-letter karaoke (try --karaoke)"));
        assert!(p.edits.is_empty());
    }

    #[test]
    fn karaoke_is_re_spaced_in_every_row_of_the_line() {
        // One row in track 2; two layers of it in track 3, each with tags of its own.
        let full = row(5000, 0, "OP", "", "go trough");
        let mut signs = row(5000, 0, "OP", r"\bord3", "go trough");
        signs.extend(row(5000, 1, "OP", r"\bord0", "go trough"));
        let p = plan_one(
            &[track(2, &full), track(3, &signs)],
            "OP",
            "go trough",
            "go through",
            true,
        );
        assert_eq!(
            p.outcomes,
            [(
                1,
                Outcome::Respaced {
                    px: 20,
                    guessed: String::new()
                }
            )]
        );
        // OP is centred: the letters before the h move 10 px left, those after it 10 px right, and
        // the h is a clone of the t, right after it, midway between it and the r.
        let e = &p.edits[&2];
        assert_eq!(e.changed.len(), 9);
        assert_eq!(payload(&p, 2, 0), r"0,0,OP,,0,0,0,,{\an5\pos(190,40)}g");
        assert_eq!(payload(&p, 2, 3), r"0,0,OP,,0,0,0,,{\an5\pos(250,40)}t");
        assert_eq!(payload(&p, 2, 4), r"0,0,OP,,0,0,0,,{\an5\pos(290,40)}r");
        assert_eq!(
            e.added[&3].after,
            [br"0,0,OP,,0,0,0,,{\an5\pos(270,40)}h".to_vec()]
        );
        assert!(e.added[&3].before.is_empty() && e.removed.is_empty());
        assert_eq!(
            p.edits[&3].added[&3].after,
            [br"0,0,OP,,0,0,0,,{\an5\bord3\pos(270,40)}h".to_vec()]
        );
        assert_eq!(
            p.edits[&3].added[&12].after,
            [br"0,1,OP,,0,0,0,,{\an5\bord0\pos(270,40)}h".to_vec()]
        );
        let rows: Vec<(usize, usize, usize, usize)> = p
            .rows
            .iter()
            .map(|r| (r.track, r.added, r.removed, r.moved))
            .collect();
        assert_eq!(rows, [(2, 1, 0, 9), (3, 1, 0, 9), (3, 1, 0, 9)]);
        assert_eq!(p.rows[0].blocks, (0..9).collect::<Vec<_>>());
    }

    #[test]
    fn a_removed_letter_closes_a_left_row_up_written_as_its_decimals() {
        // Style Left is left-aligned: only the letters after the change move. Positions are
        // written with as many decimals as the row writes them.
        for (decimals, gone) in [(".50", "280.50"), (".555", "280.555")] {
            let letters: Vec<Block> = row(5000, 0, "Left", "", "omellette")
                .into_iter()
                .map(|mut b| {
                    let p = String::from_utf8(b.payload).unwrap();
                    b.payload = p.replace(",40)", &format!("{decimals},40)")).into_bytes();
                    b
                })
                .collect();
            let p = plan_one(&[track(0, &letters)], "Left", "omellette", "omelette", true);
            assert_eq!(
                p.outcomes,
                [(
                    1,
                    Outcome::Respaced {
                        px: -20,
                        guessed: String::new()
                    }
                )]
            );
            let e = &p.edits[&0];
            assert_eq!(e.removed, [4].into());
            // The letters before it stay as they are written.
            assert_eq!(e.changed.len(), 4, "{decimals}");
            assert!(!e.changed.contains_key(&0));
            assert_eq!(
                payload(&p, 0, 5),
                format!(r"0,0,Left,,0,0,0,,{{\an5\pos({gone},40)}}e")
            );
            let r = &p.rows[0];
            assert_eq!((r.changed, r.added, r.removed, r.moved), (0, 0, 1, 4));
        }
    }

    #[test]
    fn a_letter_never_seen_in_the_style_gets_a_guessed_width() {
        let letters = row(5000, 0, "OP", "", "go trough");
        let p = plan_one(&[track(0, &letters)], "OP", "go trough", "go trouqh", true);
        assert_eq!(
            p.outcomes,
            [(
                1,
                Outcome::Respaced {
                    px: 0,
                    guessed: "q".into()
                }
            )]
        );
        // A swap: the g's event now reads q, in place.
        assert_eq!(payload(&p, 0, 7), r"0,0,OP,,0,0,0,,{\an5\pos(340,40)}q");
        let r = &p.rows[0];
        assert_eq!((r.changed, r.added, r.removed, r.moved), (1, 0, 0, 0));
    }

    #[test]
    fn a_letter_put_where_a_blank_was_takes_a_drawn_letters_tags() {
        // The blank has none of the row's tags; the letters' \k and \fscx must not be lost.
        let mut letters = row(5000, 0, "OP", r"\k20\fscx90", "go hat");
        letters[2] = block(5000, 0, "OP", r"{\an5\pos(240,40)}");
        // Added after the blank: beside it, but drawn like the o before it.
        let p = plan_one(&[track(0, &letters)], "OP", "go hat", "go that", true);
        assert_eq!(
            p.edits[&0].added[&2].after,
            [br"0,0,OP,,0,0,0,,{\an5\k20\fscx90\pos(250,40)}t".to_vec()]
        );
        // The blank's own event made a letter: the same.
        let p = plan_one(&[track(0, &letters)], "OP", "go hat", "goxhat", true);
        assert_eq!(
            payload(&p, 0, 2),
            r"0,0,OP,,0,0,0,,{\an5\k20\fscx90\pos(240,40)}x"
        );
    }

    #[test]
    fn karaoke_letters_with_coordinates_besides_pos_are_left_for_a_hand_fix() {
        // A clip, an origin or a move is placed in the frame too: re-spacing would leave it behind.
        for tag in [r"\clip(0,0,100,100)", r"\iclip(0,0,100,100)", r"\org(0,0)"] {
            let letters = row(5000, 0, "OP", tag, "go trough");
            let p = plan_one(&[track(0, &letters)], "OP", "go trough", "go through", true);
            assert_eq!(
                p.outcomes,
                hand("karaoke letters carry \\clip, \\org or \\move"),
                "{tag}"
            );
        }
    }

    #[test]
    fn karaoke_timed_letter_by_letter_is_left_for_a_hand_fix() {
        let mut letters = row(5000, 0, "OP", r"\t(0,200,\alpha&H00&)", "go trough");
        letters[4] = block(5000, 0, "OP", r"{\an5\t(100,300,\alpha&H00&)\pos(280,40)}r");
        let p = plan_one(&[track(0, &letters)], "OP", "go trough", "go through", true);
        assert_eq!(p.outcomes, hand("per-letter timing"));
        let k: Vec<Block> = (0..3)
            .map(|i| {
                let x = 200 + 20 * i;
                let c = ["a", "", "b"][i];
                block(
                    5000,
                    0,
                    "OP",
                    &format!(r"{{\an5\k{}\pos({x},40)}}{c}", 10 + i),
                )
            })
            .collect();
        let p = plan_one(&[track(0, &k)], "OP", "a b", "a c", true);
        assert_eq!(p.outcomes, hand("per-letter timing"));
    }

    #[test]
    fn karaoke_that_is_not_one_letter_per_event_is_left_for_a_hand_fix() {
        let syllables = [
            block(5000, 0, "OP", r"{\an5\pos(200,40)}go"),
            block(5000, 0, "OP", r"{\an5\pos(240,40)}"),
            block(5000, 0, "OP", r"{\an5\pos(300,40)}trough"),
        ];
        let p = plan_one(
            &[track(0, &syllables)],
            "OP",
            "go trough",
            "go through",
            true,
        );
        assert_eq!(p.outcomes, hand("karaoke not one letter per event"));
        // Without the flag too: --karaoke would not help, so it is not suggested.
        let p = plan_one(
            &[track(0, &syllables)],
            "OP",
            "go trough",
            "go through",
            false,
        );
        assert_eq!(p.outcomes, hand("karaoke not one letter per event"));
        // Two blank letters in a row: the line reads one space where the row has two.
        let gap = row(5000, 0, "OP", "", "go  trough");
        let p = plan_one(&[track(0, &gap)], "OP", "go trough", "go through", true);
        assert_eq!(p.outcomes, hand("karaoke not one letter per event"));
    }

    #[test]
    fn karaoke_letters_read_in_another_order_are_left_for_a_hand_fix() {
        // b and c round to the same pixel, so the line was read b, c; their positions say c, b.
        let letters = [
            block(5000, 0, "OP", r"{\an5\pos(60,40)}x"),
            block(5000, 0, "OP", r"{\an5\pos(80,40)}a"),
            block(5000, 0, "OP", r"{\an5\pos(100.4,40)}b"),
            block(5000, 0, "OP", r"{\an5\pos(99.6,40)}c"),
            block(5000, 0, "OP", r"{\an5\pos(120,40)}d"),
        ];
        let p = plan_one(&[track(0, &letters)], "OP", "xabcd", "xabcde", true);
        assert_eq!(p.outcomes, hand("karaoke letters overlap"));
    }

    #[test]
    fn blank_letters_need_no_tags_of_their_own() {
        // A blank draws nothing: its missing \k and \fscx are neither timing nor size.
        let mut letters = row(5000, 0, "OP", r"\k20\fscx90", "go trough");
        letters[2] = block(5000, 0, "OP", r"{\an5\pos(240,40)}");
        let p = plan_one(&[track(0, &letters)], "OP", "go trough", "go through", true);
        assert_eq!(
            p.outcomes,
            [(
                1,
                Outcome::Respaced {
                    px: 20,
                    guessed: String::new()
                }
            )]
        );
    }

    #[test]
    fn an_odd_growth_moves_both_sides_alike() {
        // Every letter 25 px wide: an h grows the row by 25, 12.5 each way, and both sides move
        // 13 px rather than one 12 and the other 13.
        let wide = |ms: i64, text: &str| -> Vec<Block> {
            text.chars()
                .enumerate()
                .map(|(i, c)| {
                    let c = if c == ' ' {
                        String::new()
                    } else {
                        c.to_string()
                    };
                    let x = 200 + 25 * i;
                    block(ms, 0, "OP", &format!(r"{{\an5\pos({x},40)}}{c}"))
                })
                .collect()
        };
        let mut letters = wide(5000, "do it");
        letters.extend(wide(9000, "hhh"));
        let p = plan_one(&[track(0, &letters)], "OP", "do it", "do hit", true);
        assert_eq!(
            p.outcomes,
            [(
                1,
                Outcome::Respaced {
                    px: 26,
                    guessed: String::new()
                }
            )]
        );
        assert_eq!(payload(&p, 0, 0), r"0,0,OP,,0,0,0,,{\an5\pos(187,40)}d");
        assert_eq!(payload(&p, 0, 4), r"0,0,OP,,0,0,0,,{\an5\pos(313,40)}t");
        // The h is laid out from the blank as written (237), not as it would be unrounded
        // (237.5); the row has grown 26, as the status says.
        assert_eq!(
            p.edits[&0].added[&2].after,
            [br"0,0,OP,,0,0,0,,{\an5\pos(262,40)}h".to_vec()]
        );
    }

    #[test]
    fn a_karaoke_row_beside_a_letter_without_pos_is_left_for_a_hand_fix() {
        // A letter of the row set moving with \move: the row of \pos letters has a hole there.
        let mut letters = row(5000, 0, "OP", "", "go trough");
        letters.push(block(5000, 0, "OP", r"{\an5\move(400,40,400,0)}s"));
        let p = plan_one(&[track(0, &letters)], "OP", "go trough", "go through", true);
        assert_eq!(p.outcomes, hand("karaoke row has a letter without \\pos"));
    }

    #[test]
    fn a_fix_that_reads_as_the_original_is_in_already() {
        // A reviewer's wording that is the line itself changes nothing: no remux for it.
        let blocks = [block(5000, 0, "D", "teh end")];
        let p = plan_one(&[track(0, &blocks)], "D", "teh end", "teh end", true);
        assert_eq!(p.outcomes, [(1, Outcome::AlreadyApplied)]);
        assert!(p.edits.is_empty());
        let letters = row(5000, 0, "OP", "", "go trough");
        let p = plan_one(&[track(0, &letters)], "OP", "go trough", "go trough", true);
        assert_eq!(p.outcomes, [(1, Outcome::AlreadyApplied)]);
        assert!(p.edits.is_empty() && p.rows.is_empty());
    }

    #[test]
    fn karaoke_spaced_unlike_its_style_is_left_for_a_hand_fix() {
        // Another row of the style, its letters twice as far apart: no widths explain both.
        let mut letters = row(5000, 0, "OP", "", "go trough");
        letters.extend(row(9000, 0, "OP", "", "go go").into_iter().enumerate().map(
            |(i, mut b)| {
                let p = String::from_utf8(b.payload).unwrap();
                let x = 200 + 40 * i;
                b.payload = p
                    .replace(&format!("({},40)", 200 + 20 * i), &format!("({x},40)"))
                    .into_bytes();
                b
            },
        ));
        let p = plan_one(&[track(0, &letters)], "OP", "go trough", "go through", true);
        assert_eq!(p.outcomes, hand("karaoke letter widths do not fit"));
    }

    /// `blocks` with each payload's `from` replaced by `to`.
    fn retagged(blocks: Vec<Block>, from: &str, to: &str) -> Vec<Block> {
        blocks
            .into_iter()
            .map(|mut b| {
                let p = String::from_utf8(b.payload).unwrap();
                b.payload = p.replace(from, to).into_bytes();
                b
            })
            .collect()
    }

    #[test]
    fn karaoke_letters_not_centred_on_their_pos_are_left_for_a_hand_fix() {
        let not_centred = hand("karaoke letters not centred on their \\pos");
        // Each letter's \pos is its left edge: their spacing is not the half-widths it is read as.
        let left = retagged(row(5000, 0, "OP", "", "go trough"), r"\an5", r"\an7");
        let p = plan_one(&[track(0, &left)], "OP", "go trough", "go through", true);
        assert_eq!(p.outcomes, not_centred);
        // No tag of their own, in a left-aligned style: the same.
        let plain = retagged(row(5000, 0, "Left", "", "go trough"), r"\an5", "");
        let p = plan_one(&[track(0, &plain)], "Left", "go trough", "go through", true);
        assert_eq!(p.outcomes, not_centred);
        // An \an libass cannot use comes first: it claims the line for the style's alignment, and
        // the \an5 after it is ignored.
        let first = retagged(row(5000, 0, "Left", "", "go trough"), r"\an5", r"\an0\an5");
        let p = plan_one(&[track(0, &first)], "Left", "go trough", "go through", true);
        assert_eq!(p.outcomes, not_centred);
        // No tag of their own, in a centred style: centred.
        let plain = retagged(row(5000, 0, "OP", "", "go trough"), r"\an5", "");
        let p = plan_one(&[track(0, &plain)], "OP", "go trough", "go through", true);
        assert_eq!(
            p.outcomes,
            [(
                1,
                Outcome::Respaced {
                    px: 20,
                    guessed: String::new()
                }
            )]
        );
    }

    #[test]
    fn a_width_guessed_in_any_row_of_the_fix_is_said() {
        // Track 2's style shows an h elsewhere; track 3's never does, so its h is a guess.
        let mut full = row(5000, 0, "OP", "", "go troug");
        full.extend(row(9000, 0, "OP", "", "ha ha"));
        let signs = row(5000, 0, "OP", "", "go troug");
        let p = plan_one(
            &[track(2, &full), track(3, &signs)],
            "OP",
            "go troug",
            "go trough",
            true,
        );
        assert_eq!(
            p.outcomes,
            [(
                1,
                Outcome::Respaced {
                    px: 20,
                    guessed: "h".into()
                }
            )]
        );
    }

    #[test]
    fn rows_drawn_at_another_size_or_missing_a_letter_are_not_learnt_from() {
        let spread = |blocks: Vec<Block>| -> Vec<Block> {
            blocks
                .into_iter()
                .enumerate()
                .map(|(i, mut b)| {
                    let p = String::from_utf8(b.payload).unwrap();
                    let x = 200 + 40 * i;
                    b.payload = p
                        .replace(&format!("({},40)", 200 + 20 * i), &format!("({x},40)"))
                        .into_bytes();
                    b
                })
                .collect()
        };
        let respaced = [(
            1,
            Outcome::Respaced {
                px: 20,
                guessed: String::new(),
            },
        )];
        // Another row at twice the width, as its \fscx says: the fit leaves it out.
        let mut letters = row(5000, 0, "OP", "", "go trough");
        letters.extend(spread(row(9000, 0, "OP", r"\fscx200", "go go")));
        let p = plan_one(&[track(0, &letters)], "OP", "go trough", "go through", true);
        assert_eq!(p.outcomes, respaced);
        // Another row whose letters sit on their \pos by their left edges: the same.
        let mut letters = row(5000, 0, "OP", "", "go trough");
        letters.extend(spread(retagged(
            row(9000, 0, "OP", "", "go go"),
            r"\an5",
            r"\an7",
        )));
        let p = plan_one(&[track(0, &letters)], "OP", "go trough", "go through", true);
        assert_eq!(p.outcomes, respaced);
        // Another row with a letter moving (no \pos): its gap is no pair of neighbours.
        let mut letters = row(5000, 0, "OP", "", "go trough");
        let mut holed = row(9000, 0, "OP", "", "oo oo oo oo");
        for i in [1, 4, 7, 10] {
            let x = 200 + 20 * i;
            holed[i] = block(9000, 0, "OP", &format!(r"{{\an5\move({x},40,{x},0)}}o"));
        }
        letters.extend(holed);
        let p = plan_one(&[track(0, &letters)], "OP", "go trough", "go through", true);
        assert_eq!(p.outcomes, respaced);
        // The row being fixed drawn at two sizes: no widths for it.
        let mut mixed = row(5000, 0, "OP", "", "go trough");
        mixed[0] = block(5000, 0, "OP", r"{\an5\fscx150\pos(200,40)}g");
        let p = plan_one(&[track(0, &mixed)], "OP", "go trough", "go through", true);
        assert_eq!(p.outcomes, hand("karaoke letters drawn at different sizes"));
    }

    #[test]
    fn karaoke_left_for_a_hand_fix_for_its_style_or_the_whole_line() {
        let letters = row(5000, 0, "Gone", "", "go trough");
        let p = plan_one(
            &[track(0, &letters)],
            "Gone",
            "go trough",
            "go through",
            true,
        );
        assert_eq!(p.outcomes, hand("its style is not in the header"));
        let letters = row(5000, 0, "OP", "", "abc");
        let p = plan_one(&[track(0, &letters)], "OP", "abc", "xyz", true);
        assert_eq!(p.outcomes, hand("the change covers the whole line"));
    }

    #[test]
    fn karaoke_reading_the_fix_already_is_not_made_again() {
        let letters = row(5000, 0, "OP", "", "go through");
        let p = plan_one(&[track(0, &letters)], "OP", "go trough", "go through", true);
        assert_eq!(p.outcomes, [(1, Outcome::AlreadyApplied)]);
        assert!(p.edits.is_empty() && p.rows.is_empty());
    }

    #[test]
    fn two_fixes_of_one_karaoke_row_are_both_left_for_a_hand_fix() {
        let letters = row(5000, 0, "OP", "", "go trough");
        let f1 = finding(1, "OP", "go trough", &[(0, 5000)]);
        let f2 = finding(2, "OP", "go trough", &[(0, 5000)]);
        let fixes = [
            Fix {
                finding: &f1,
                text: "go through".into(),
            },
            Fix {
                finding: &f2,
                text: "so trough".into(),
            },
        ];
        let p = plan(&[track(0, &letters)], &fixes, true);
        let both = Outcome::ByHand("two fixes on one line".into());
        assert_eq!(p.outcomes, [(1, both.clone()), (2, both)]);
        assert!(p.edits.is_empty() && p.rows.is_empty());
    }

    #[test]
    fn new_blocks_go_beside_the_block_they_were_cloned_from_with_its_times() {
        let blocks = [block(1000, 0, "D", "a"), block(2000, 0, "D", "b")];
        let mut e = Edits::default();
        e.removed.insert(0);
        e.added.insert(
            1,
            Added {
                before: vec![b"x".to_vec()],
                after: vec![b"y".to_vec(), b"z".to_vec()],
            },
        );
        let got: Vec<(i64, String)> = edited(&blocks, &e)
            .into_iter()
            .map(|b| (b.timestamp, String::from_utf8(b.payload).unwrap()))
            .collect();
        assert_eq!(
            got,
            [
                (2000, "x".into()),
                (2000, "0,0,D,,0,0,0,,b".into()),
                (2000, "y".into()),
                (2000, "z".into())
            ]
        );
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
            plan(&[track(0, &blocks)], &[fix], false).outcomes,
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
        let p = plan(&[track(0, &blocks)], &fixes, false);
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
            plan(&[track(0, &blocks)], &[fix], false).outcomes,
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
        let p = plan(&[track(0, &blocks)], &[fix], false);
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
            plan(&[t], &fixes, false).outcomes,
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
        let p = plan(&[track(0, std::slice::from_ref(&b))], &[fix], false);
        assert_eq!(p.edits[&0].changed[&0], b"0,0,D,,0,0,0,,the end");
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
