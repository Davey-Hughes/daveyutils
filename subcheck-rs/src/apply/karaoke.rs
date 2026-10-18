//! Per-letter karaoke: a row of events, one letter each, each centred on its `\pos`. A fix changes
//! the row's letters and re-spaces it by each letter's width, solved from every such row of the
//! style: the subtitles were laid out by a font's metrics, and their positions still hold them.

use std::collections::HashMap;

use crate::text::is_number;

/// A row's letters in x order: each one's x (its centre) and character, a space for an empty event.
pub type Row = Vec<(f64, char)>;

/// How much a character's width is pulled toward the median distance: enough to settle one the
/// rows cannot pin down, too little to move one they can.
const PULL: f64 = 1e-3;

/// How far an adjacent distance may be from what the widths say before a row is said not to fit:
/// whole-pixel positions are off by up to 1 px between two letters.
pub const FIT_PX: f64 = 2.0;

/// Each character's width in one style, solved from its rows' letter spacing.
#[derive(Debug, Clone, PartialEq)]
pub struct Widths {
    known: HashMap<char, f64>,
    /// The median distance between adjacent letters: what a character never seen is taken to be.
    median: f64,
}

impl Widths {
    /// The widths that best explain `rows`, where adjacent letters' centres are `(w_a + w_b) / 2`
    /// apart: least squares over every adjacent pair, each width pulled a little toward the median.
    pub fn fit(rows: &[Row]) -> Widths {
        let mut chars: Vec<char> = rows.iter().flatten().map(|&(_, c)| c).collect();
        chars.sort_unstable();
        chars.dedup();
        let index: HashMap<char, usize> = chars.iter().enumerate().map(|(i, &c)| (c, i)).collect();
        let pairs: Vec<(usize, usize, f64)> = rows
            .iter()
            .flat_map(|r| r.windows(2))
            .map(|w| (index[&w[0].1], index[&w[1].1], w[1].0 - w[0].0))
            .collect();
        let median = median(pairs.iter().map(|p| p.2).collect());
        // The normal equations (AᵀA + PULL·I) w = Aᵀd + PULL·median, where each pair is a row of A
        // with ½ for each of its two letters.
        let n = chars.len();
        let mut a = vec![vec![0.0; n]; n];
        let mut b = vec![0.0; n];
        for &(i, j, d) in &pairs {
            for (p, q) in [(i, i), (j, j), (i, j), (j, i)] {
                a[p][q] += 0.25;
            }
            b[i] += d / 2.0;
            b[j] += d / 2.0;
        }
        for (i, row) in a.iter_mut().enumerate() {
            row[i] += PULL;
            b[i] += PULL * median;
        }
        Widths {
            known: chars.into_iter().zip(solve(a, b)).collect(),
            median,
        }
    }

    /// `c`'s width, and whether the rows showed it (else it is the median distance).
    pub fn of(&self, c: char) -> (f64, bool) {
        self.known
            .get(&c)
            .map_or((self.median, false), |&w| (w, true))
    }

    /// Whether every adjacent distance in `row` is within [`FIT_PX`] of what the widths say.
    pub fn fits(&self, row: &Row) -> bool {
        row.windows(2).all(|w| {
            let expected = f64::midpoint(self.of(w[0].1).0, self.of(w[1].1).0);
            (w[1].0 - w[0].0 - expected).abs() <= FIT_PX
        })
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        f64::midpoint(v[m - 1], v[m])
    }
}

/// The `x` with `matrix · x = rhs`, by Gaussian elimination with partial pivoting; `matrix` is
/// square and not singular.
fn solve(mut matrix: Vec<Vec<f64>>, mut rhs: Vec<f64>) -> Vec<f64> {
    let size = rhs.len();
    for col in 0..size {
        let pivot_row = (col..size)
            .max_by(|&i, &j| matrix[i][col].abs().total_cmp(&matrix[j][col].abs()))
            .expect("col < size");
        matrix.swap(col, pivot_row);
        rhs.swap(col, pivot_row);
        let (top, rest) = matrix.split_at_mut(col + 1);
        let pivot = &top[col];
        for (below, row) in rest.iter_mut().enumerate() {
            let factor = row[col] / pivot[col];
            for (value, above) in row[col..].iter_mut().zip(&pivot[col..]) {
                *value -= factor * above;
            }
            rhs[col + 1 + below] -= factor * rhs[col];
        }
    }
    let mut solution = vec![0.0; size];
    for col in (0..size).rev() {
        let known: f64 = (col + 1..size).map(|j| matrix[col][j] * solution[j]).sum();
        solution[col] = (rhs[col] - known) / matrix[col][col];
    }
    solution
}

/// Where a row is anchored, from its style's Alignment: the row stays put there as it grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Centre,
    Right,
}

/// What a style whose Alignment cannot be read is left for a hand fix with.
const BAD_ALIGNMENT: &str = "its style's Alignment is not one subcheck knows";

/// The Alignment of `style` in an ASS or SSA header, as left, centre or right, read as libass reads
/// it: the last definition of the name wins, a leading `*` is no part of a name, and the numbers
/// are the keypad's under `[V4+ Styles]` and SSA's under `[V4 Styles]`. An error, the reason to
/// leave the fix for a hand fix, when there is no such style or its Alignment is not one of the
/// format's.
pub fn alignment(header: &[u8], style: &str) -> Result<Align, &'static str> {
    let header = String::from_utf8_lossy(header);
    let name = style.trim_start_matches('*');
    let mut format: Option<Vec<String>> = None;
    // In a styles section, and whether it numbers alignments as SSA does.
    let mut section: Option<bool> = None;
    // The last definition's Alignment (if it parses) and numbering.
    let mut found: Option<(Option<u32>, bool)> = None;
    for line in header.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = if line.eq_ignore_ascii_case("[V4+ Styles]") {
                Some(false)
            } else if line.eq_ignore_ascii_case("[V4 Styles]") {
                Some(true)
            } else {
                None
            };
            continue;
        }
        let Some(legacy) = section else {
            continue;
        };
        if let Some(rest) = line.strip_prefix("Format:") {
            format = Some(rest.split(',').map(|f| f.trim().to_lowercase()).collect());
        } else if let (Some(rest), Some(format)) = (line.strip_prefix("Style:"), &format) {
            let fields: Vec<&str> = rest.splitn(format.len(), ',').map(str::trim).collect();
            let field = |name: &str| {
                format
                    .iter()
                    .position(|f| f == name)
                    .and_then(|i| fields.get(i))
            };
            if field("name").map(|n| n.trim_start_matches('*')) == Some(name) {
                found = Some((field("alignment").and_then(|a| a.parse().ok()), legacy));
            }
        }
    }
    let (n, legacy) = found.ok_or("its style is not in the header")?;
    column(n.ok_or(BAD_ALIGNMENT)?, legacy).ok_or(BAD_ALIGNMENT)
}

/// Left, centre or right from an alignment number. ASS numbers the keypad (1-9); SSA, and the
/// legacy `\a` tag, have 1-3 at the bottom, +4 at the top and +8 midway. `None` for any other.
fn column(n: u32, legacy: bool) -> Option<Align> {
    let column = if legacy {
        (n & 3).checked_sub(1).filter(|_| n <= 11)?
    } else {
        n.checked_sub(1).filter(|_| n <= 9)? % 3
    };
    Some([Align::Left, Align::Centre, Align::Right][column as usize])
}

/// What an `\an` or `\a` tag (as `text::tags` gives it, without its backslash) does to where a
/// letter sits on its `\pos`, as libass reads it: `Some(Some(_))` where it puts it; `Some(None)`
/// for a number libass cannot use, which still claims the line (it keeps its style's alignment and
/// later alignment tags are ignored); `None` for any other tag.
pub fn tag_alignment(tag: &str) -> Option<Option<Align>> {
    let (n, legacy) = match tag.strip_prefix("an") {
        Some(n) => (n, false),
        None => (tag.strip_prefix('a')?, true),
    };
    if !n.is_empty() && !is_number(n) {
        return None;
    }
    let n: u32 = n.parse().unwrap_or(0);
    // VSFilter reads \a4 and \a8 as \a5, top left.
    if legacy && (1..=11).contains(&n) && n.is_multiple_of(4) {
        return Some(Some(Align::Left));
    }
    Some(column(n, legacy))
}

/// Whether a tag (without its backslash) changes how wide letters are drawn: a font, its size, x
/// scale or spacing, bold, italic, or a reset to a style.
pub fn widens(tag: &str) -> bool {
    (tag.starts_with("fs") && !tag.starts_with("fscy"))
        || tag.starts_with("fn")
        || tag.starts_with('r')
        || tag.strip_prefix('b').is_some_and(is_number)
        || tag.strip_prefix('i').is_some_and(is_number)
}

/// A letter of a re-spaced row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Letter {
    /// The old row's letter at this index.
    Old(usize),
    /// A new one, cloned from the old row's letter at `from`, to go right after it (or before it).
    New { from: usize, after: bool },
}

/// The new row: each letter's source, x and character, in x order; and how much wider it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Respaced {
    pub letters: Vec<(Letter, f64, char)>,
    pub growth: f64,
    /// Characters the rows never showed, whose width is a guess.
    pub guessed: Vec<char>,
}

/// `row` with its letters `from..to` replaced by `new`, re-spaced by `widths` about `align`: the
/// letters before the change move left and those after it right (as the anchor says) by the
/// growth, and the new letters are laid out by their widths from the letter before them (after
/// them, when the change starts the row). Each side's shift is put through `round` (to the
/// precision the row is written in) first, so the new letters sit where their neighbours will be
/// written, and the growth returned is the one written. The change's letters take the old ones'
/// events in order; any more are cloned from the letter before them, or after them at the row's
/// start. `None` when the change covers the whole row.
pub fn respace(
    row: &Row,
    from: usize,
    to: usize,
    new: &[char],
    widths: &Widths,
    align: Align,
    round: impl Fn(f64) -> f64,
) -> Option<Respaced> {
    if from == 0 && to == row.len() {
        return None;
    }
    let w = |c: char| widths.of(c).0;
    let growth: f64 =
        new.iter().map(|&c| w(c)).sum::<f64>() - row[from..to].iter().map(|l| w(l.1)).sum::<f64>();
    let (left, right) = match align {
        Align::Left => (0.0, round(growth)),
        Align::Centre => (round(-growth / 2.0), round(growth / 2.0)),
        Align::Right => (round(-growth), 0.0),
    };
    let growth = right - left;
    let xs: Vec<f64> = if from > 0 {
        let mut edge = row[from - 1].0 + left + w(row[from - 1].1) / 2.0;
        new.iter()
            .map(|&c| {
                edge += w(c);
                edge - w(c) / 2.0
            })
            .collect()
    } else {
        let mut edge = row[to].0 + right - w(row[to].1) / 2.0;
        let mut xs: Vec<f64> = new
            .iter()
            .rev()
            .map(|&c| {
                edge -= w(c);
                edge + w(c) / 2.0
            })
            .collect();
        xs.reverse();
        xs
    };
    let reused = new.len().min(to - from);
    let clone = if reused > 0 {
        Letter::New {
            from: from + reused - 1,
            after: true,
        }
    } else if from > 0 {
        Letter::New {
            from: from - 1,
            after: true,
        }
    } else {
        Letter::New {
            from: to,
            after: false,
        }
    };
    let mut letters: Vec<(Letter, f64, char)> = row[..from]
        .iter()
        .enumerate()
        .map(|(i, &(x, c))| (Letter::Old(i), x + left, c))
        .collect();
    for (k, (&c, x)) in new.iter().zip(xs).enumerate() {
        let letter = if k < reused {
            Letter::Old(from + k)
        } else {
            clone
        };
        letters.push((letter, x, c));
    }
    letters.extend(
        row[to..]
            .iter()
            .enumerate()
            .map(|(i, &(x, c))| (Letter::Old(to + i), x + right, c)),
    );
    let mut guessed: Vec<char> = new.iter().copied().filter(|&c| !widths.of(c).1).collect();
    guessed.sort_unstable();
    guessed.dedup();
    Some(Respaced {
        letters,
        growth,
        guessed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row of `text` with letters `step` px apart from `x0`.
    fn even(text: &str, x0: f64, step: f64) -> Row {
        (0..)
            .zip(text.chars())
            .map(|(i, c)| (x0 + step * f64::from(i), c))
            .collect()
    }

    fn xs(r: &Respaced) -> Vec<i64> {
        #[allow(clippy::cast_possible_truncation)]
        r.letters.iter().map(|l| l.1.round() as i64).collect()
    }

    #[test]
    fn widths_come_from_letter_spacing() {
        // "aa" 10 apart says a = 10; "ab" 15 apart then says b = 20.
        let rows = [vec![(0.0, 'a'), (10.0, 'a'), (25.0, 'b'), (45.0, 'b')]];
        let w = Widths::fit(&rows);
        assert!((w.of('a').0 - 10.0).abs() < 0.05, "{w:?}");
        assert!((w.of('b').0 - 20.0).abs() < 0.05, "{w:?}");
        assert!(w.of('a').1);
        // Never seen: the median distance (10, 15, 20), and said to be a guess.
        assert_eq!(w.of('z'), (15.0, false));
        assert!(w.fits(&rows[0]));
        assert!(!w.fits(&vec![(0.0, 'a'), (13.0, 'a')]));
    }

    /// K-On!! E01's OP row with the typo, as the subtitles place it (track 6, 0:02:58.03).
    fn trough() -> Row {
        let x = [
            315, 345, 372, 393, 416, 441, 467, 491, 511, 534, 559, 582, 602, 621, 642, 664, 689,
            716, 743, 771, 795, 815, 839, 861, 883, 911, 936, 957, 990, 1020, 1042, 1072, 1097,
            1120, 1148, 1179, 1204, 1233, 1266, 1287, 1305, 1325, 1349, 1373, 1391, 1412, 1435,
            1456, 1483, 1506, 1526, 1553, 1576, 1592, 1609,
        ];
        let text = "The beat gets trough to our mind and we freely enjoy it";
        assert_eq!(text.chars().count(), x.len());
        text.chars()
            .zip(x)
            .map(|(c, x)| (f64::from(x), c))
            .collect()
    }

    #[test]
    fn an_inserted_letter_spreads_a_centred_row_by_its_width() {
        let row = trough();
        let w = Widths::fit(std::slice::from_ref(&row));
        // "gets t|rough": the h goes after letter 14 (the t at 642).
        let r = respace(&row, 15, 15, &['h'], &w, Align::Centre, f64::round).unwrap();
        // Each side moves by half the h's width (14.83) rounded to 15: the row grows 30.
        assert!((r.growth - 30.0).abs() < 1e-9, "{}", r.growth);
        let x = xs(&r);
        // From this one row: t 627, h 653, r 679. All 25 OP rows of the track give 628, 653, 678,
        // which is where the E02 hand fix put them.
        assert_eq!(&x[14..17], [627, 653, 679]);
        assert_eq!(x[0], 315 - 15);
        assert_eq!(x[55], 1609 + 15);
        assert_eq!(
            r.letters[15].0,
            Letter::New {
                from: 14,
                after: true
            }
        );
        assert_eq!(r.letters[16].0, Letter::Old(15));
        assert_eq!(r.guessed, Vec::<char>::new());
    }

    #[test]
    fn a_left_row_grows_right_and_a_right_row_grows_left() {
        let row = even("go trough", 100.0, 20.0);
        let w = Widths::fit(std::slice::from_ref(&row));
        let left = respace(&row, 4, 4, &['h'], &w, Align::Left, f64::round).unwrap();
        assert_eq!(
            xs(&left),
            [100, 120, 140, 160, 180, 200, 220, 240, 260, 280]
        );
        let right = respace(&row, 4, 4, &['h'], &w, Align::Right, f64::round).unwrap();
        assert_eq!(
            xs(&right),
            [80, 100, 120, 140, 160, 180, 200, 220, 240, 260]
        );
    }

    #[test]
    fn a_removed_letter_closes_up_and_a_swap_keeps_its_event() {
        let row = even("omellette", 100.0, 20.0);
        let w = Widths::fit(std::slice::from_ref(&row));
        // "omel|lette" -> "omelette": one l goes; the sides close up by 10 each.
        let r = respace(&row, 4, 5, &[], &w, Align::Centre, f64::round).unwrap();
        assert_eq!(xs(&r), [110, 130, 150, 170, 190, 210, 230, 250]);
        assert!(r.letters.iter().all(|l| l.0 != Letter::Old(4)));
        // "teh" -> "the": two letters change, their events stay, in place.
        let row = even("teh end", 100.0, 20.0);
        let w = Widths::fit(std::slice::from_ref(&row));
        let r = respace(&row, 1, 3, &['h', 'e'], &w, Align::Centre, f64::round).unwrap();
        assert_eq!(xs(&r), [100, 120, 140, 160, 180, 200, 220]);
        assert_eq!((r.letters[1].0, r.letters[1].2), (Letter::Old(1), 'h'));
    }

    #[test]
    fn a_letter_at_the_start_is_cloned_from_the_one_after_it() {
        let row = even("ave", 100.0, 20.0);
        let w = Widths::fit(std::slice::from_ref(&row));
        let r = respace(&row, 0, 0, &['H'], &w, Align::Centre, f64::round).unwrap();
        assert_eq!(
            r.letters[0].0,
            Letter::New {
                from: 0,
                after: false
            }
        );
        // H is never seen: it is the median, and a guess.
        assert_eq!(xs(&r), [90, 110, 130, 150]);
        assert_eq!(r.guessed, ['H']);
        // Changing every letter leaves nothing to anchor to.
        assert_eq!(
            respace(&row, 0, 3, &['x'], &w, Align::Centre, f64::round),
            None
        );
    }

    const STYLES: &str = "[Script Info]\nPlayResX: 1920\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: OP en,Segoe Print,95,&H00FFFFFF,&H000019FF,&H00151A60,&H00000000,-1,0,0,0,85,100,0,0,1,2,2,2,10,10,20,1\nStyle: Left,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,7,10,10,10,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";

    #[test]
    fn the_alignment_comes_from_the_style() {
        let h = STYLES.as_bytes();
        assert_eq!(alignment(h, "OP en"), Ok(Align::Centre));
        assert_eq!(alignment(h, "Left"), Ok(Align::Left));
        assert_eq!(
            alignment(h, "Missing"),
            Err("its style is not in the header")
        );
        // SSA: 3 is bottom right, 11 (3 + 8) midway right, 5 (1 + 4) top left.
        for (n, want) in [(3, Align::Right), (11, Align::Right), (5, Align::Left)] {
            let ssa = format!("[V4 Styles]\nFormat: Name, Alignment\nStyle: S,{n}\n");
            assert_eq!(alignment(ssa.as_bytes(), "S"), Ok(want));
        }
    }

    #[test]
    fn a_style_defined_twice_is_read_as_libass_reads_it() {
        // The last definition wins, and a leading * is no part of the name.
        let twice = "[V4+ Styles]\nFormat: Name, Alignment\nStyle: OP,7\nStyle: *OP,2\n";
        assert_eq!(alignment(twice.as_bytes(), "OP"), Ok(Align::Centre));
        assert_eq!(alignment(twice.as_bytes(), "*OP"), Ok(Align::Centre));
        // An Alignment that is not one of the format's is said as such, not as a missing style.
        for (section, n) in [("V4+", "0"), ("V4+", "10"), ("V4", "4"), ("V4+", "x")] {
            let bad = format!("[{section} Styles]\nFormat: Name, Alignment\nStyle: OP,{n}\n");
            assert_eq!(
                alignment(bad.as_bytes(), "OP"),
                Err("its style's Alignment is not one subcheck knows"),
                "{n}"
            );
        }
    }

    #[test]
    fn the_numbering_follows_the_styles_section() {
        // 5 is the keypad's middle centre under [V4+ Styles], SSA's top left under [V4 Styles],
        // whatever the track's codec says.
        let ass = "[V4+ Styles]\nFormat: Name, Alignment\nStyle: S,5\n";
        let ssa = "[V4 Styles]\nFormat: Name, Alignment\nStyle: S,5\n";
        assert_eq!(alignment(ass.as_bytes(), "S"), Ok(Align::Centre));
        assert_eq!(alignment(ssa.as_bytes(), "S"), Ok(Align::Left));
    }

    #[test]
    fn a_letters_own_alignment_tag_is_an_or_the_legacy_a() {
        assert_eq!(tag_alignment("an7"), Some(Some(Align::Left)));
        assert_eq!(tag_alignment("an5"), Some(Some(Align::Centre)));
        // \a numbers as SSA does: 2 bottom centre, 3 bottom right, 9 midway left; \a4 and \a8
        // are read as \a5 (top left), as VSFilter reads them.
        assert_eq!(tag_alignment("a2"), Some(Some(Align::Centre)));
        assert_eq!(tag_alignment("a3"), Some(Some(Align::Right)));
        assert_eq!(tag_alignment("a9"), Some(Some(Align::Left)));
        assert_eq!(tag_alignment("a4"), Some(Some(Align::Left)));
        // One libass cannot use still claims the line, for the style's alignment.
        for style in ["an0", "an10", "an", "a12", "a"] {
            assert_eq!(tag_alignment(style), Some(None), "{style}");
        }
        for not in ["alpha&H00&", "pos(1,2)", "anything"] {
            assert_eq!(tag_alignment(not), None, "{not}");
        }
    }

    #[test]
    fn the_tags_that_change_a_letters_width_are_its_font_size_and_style() {
        for wide in [
            "fs40", "fs+2", "fscx120", "fsp2", "fnArial", "b1", "i1", "r", "rOP",
        ] {
            assert!(widens(wide), "{wide}");
        }
        for not in [
            "be2",
            "bord3",
            "blur1",
            "fscy120",
            "alpha&H00&",
            "an5",
            "pos(1,2)",
            "iclip(1,2,3,4)",
        ] {
            assert!(!widens(not), "{not}");
        }
    }
}
