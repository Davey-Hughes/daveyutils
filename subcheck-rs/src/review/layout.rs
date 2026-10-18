//! Where each part of the screen is, and which key each button presses. Pure, so the view draws
//! with the same numbers that `update` maps a click with.

use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::text::Span;

/// One item of a bar: its text, and for a button the key a click presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    pub text: &'static str,
    pub key: Option<KeyCode>,
}

const fn button(text: &'static str, key: KeyCode) -> Item {
    Item {
        text,
        key: Some(key),
    }
}

/// The key bar. `j/k move` is only a reminder: the wheel moves the selection.
pub const KEYS: &[Item] = &[
    Item {
        text: "j/k move",
        key: None,
    },
    button("o mpv", KeyCode::Char('o')),
    button("a accept", KeyCode::Char('a')),
    button("r reject", KeyCode::Char('r')),
    button("e edit", KeyCode::Char('e')),
    button("u undo", KeyCode::Char('u')),
    button("⏎ expand", KeyCode::Enter),
    button("f filter", KeyCode::Char('f')),
    button("n next", KeyCode::Char('n')),
    button("p prev", KeyCode::Char('p')),
    button("q quit", KeyCode::Char('q')),
];

/// The key bar while the editor is open.
pub const EDIT_KEYS: &[Item] = &[
    button("⏎ accept", KeyCode::Enter),
    button("Esc cancel", KeyCode::Esc),
];

/// The playback buttons, on the right of the status line.
pub const PLAYBACK: &[Item] = &[
    button("␣ pause", KeyCode::Char(' ')),
    button("← −5s", KeyCode::Left),
    button("→ +5s", KeyCode::Right),
    button(", −1s", KeyCode::Char(',')),
    button(". +1s", KeyCode::Char('.')),
];

const SEP: &str = " · ";

/// The parts of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    pub list: Rect,
    pub detail: Rect,
    /// The status line, which the editor takes while it is open.
    pub status: Rect,
    pub keys: Rect,
}

pub fn panes(area: Rect) -> Panes {
    // The key bar takes as many rows as it needs to show every button.
    let bar = u16::try_from(bar_rows(KEYS, area.width).len()).unwrap_or(u16::MAX);
    let [body, status, keys] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(bar),
        ])
        .areas(area);
    let [list, detail] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .areas(body);
    Panes {
        list,
        detail,
        status,
        keys,
    }
}

/// The list's entry rows: inside its border.
pub fn list_rows(p: &Panes) -> Rect {
    p.list.inner(Margin::new(1, 1))
}

/// A bar as text: its items, separated.
pub fn bar_text(items: &[Item]) -> String {
    items.iter().map(|i| i.text).collect::<Vec<_>>().join(SEP)
}

/// A bar's items in rows of at most `columns`, broken only between items; an item wider than that
/// gets a row to itself.
pub fn bar_rows(items: &[Item], columns: u16) -> Vec<&[Item]> {
    let mut rows = Vec::new();
    let (mut start, mut used) = (0, 0);
    for (n, item) in items.iter().enumerate() {
        let w = width(item.text);
        if n > start && used + width(SEP) + w > columns {
            rows.push(&items[start..n]);
            (start, used) = (n, 0);
        }
        used += if n > start { width(SEP) + w } else { w };
    }
    if start < items.len() {
        rows.push(&items[start..]);
    }
    rows
}

/// The key a click at `at` presses, on a bar flowed into `area` as [`bar_rows`] does.
pub fn bar_click(items: &[Item], area: Rect, at: Position) -> Option<KeyCode> {
    if !area.contains(at) {
        return None;
    }
    let rows = bar_rows(items, area.width);
    bar_hit(rows.get(usize::from(at.y - area.y))?, area.x, at.x)
}

/// The key a click at column `x` presses, on a bar drawn from column `left`.
pub fn bar_hit(items: &[Item], left: u16, x: u16) -> Option<KeyCode> {
    let mut at = left;
    for (n, item) in items.iter().enumerate() {
        if n > 0 {
            at = at.saturating_add(width(SEP));
        }
        let end = at.saturating_add(width(item.text));
        if (at..end).contains(&x) {
            return item.key;
        }
        at = end;
    }
    None
}

/// Where the playback buttons start on the status line `status`: flush right.
pub fn playback_left(status: Rect) -> u16 {
    status
        .right()
        .saturating_sub(width(&bar_text(PLAYBACK)))
        .max(status.x)
}

/// Whether the playback buttons show on the status line `status` beside the message `text`. They
/// need 40 columns, and give way to a message too long to fit to their left with a column clear.
pub fn playback_shown(status: Rect, text: Option<&str>) -> bool {
    let room = (playback_left(status) - status.x).saturating_sub(1);
    status.width > width(&bar_text(PLAYBACK)) && text.map_or(0, width) <= room
}

/// Columns `s` takes on screen.
pub fn width(s: &str) -> u16 {
    u16::try_from(Span::raw(s).width()).unwrap_or(u16::MAX)
}

/// The list's scroll offset: `scroll`, moved only as far as it takes to show `selected` among
/// `rows` visible rows of a list of `len`. With no rows there is nothing to keep in view.
pub fn keep_in_view(scroll: usize, selected: usize, rows: usize, len: usize) -> usize {
    if rows == 0 {
        return scroll;
    }
    // No further than the last full screen: a list that got shorter (a filter, a decided row
    // leaving the pending list) or a screen that got taller shows the entries above, as the list
    // never fills rows upward from its offset.
    let scroll = scroll.min(len.saturating_sub(rows));
    if selected < scroll {
        selected
    } else if selected >= scroll + rows {
        selected + 1 - rows
    } else {
        scroll
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_footer_is_two_lines_under_the_panes() {
        // Wide enough for the key bar's 105 columns on one row.
        let p = panes(Rect::new(0, 0, 110, 20));
        assert_eq!(p.list, Rect::new(0, 0, 55, 18));
        assert_eq!(p.detail, Rect::new(55, 0, 55, 18));
        assert_eq!(p.status, Rect::new(0, 18, 110, 1));
        assert_eq!(p.keys, Rect::new(0, 19, 110, 1));
        assert_eq!(list_rows(&p), Rect::new(1, 1, 53, 16));
    }

    #[test]
    fn a_click_on_a_bar_presses_the_button_under_it() {
        // "j/k move · o mpv · a accept …": `o mpv` is columns 11 to 15.
        assert_eq!(bar_hit(KEYS, 0, 0), None, "a reminder, not a button");
        assert_eq!(bar_hit(KEYS, 0, 9), None, "the separator");
        assert_eq!(bar_hit(KEYS, 0, 11), Some(KeyCode::Char('o')));
        assert_eq!(bar_hit(KEYS, 0, 15), Some(KeyCode::Char('o')));
        assert_eq!(bar_hit(KEYS, 0, 16), None);
        assert_eq!(bar_hit(KEYS, 0, 19), Some(KeyCode::Char('a')));
        // `⏎` is one column wide, as drawn.
        let text = bar_text(KEYS);
        let expand = u16::try_from(text[..text.find("⏎").unwrap()].chars().count()).unwrap();
        assert_eq!(bar_hit(KEYS, 0, expand), Some(KeyCode::Enter));
        assert_eq!(bar_hit(KEYS, 0, expand + 9), None);
        assert_eq!(bar_hit(KEYS, 0, width(&text) - 1), Some(KeyCode::Char('q')));
        assert_eq!(bar_hit(KEYS, 0, width(&text)), None);
        // Drawn from column 4, everything moves 4 to the right.
        assert_eq!(bar_hit(EDIT_KEYS, 4, 3), None);
        assert_eq!(bar_hit(EDIT_KEYS, 4, 4), Some(KeyCode::Enter));
        assert_eq!(bar_hit(EDIT_KEYS, 4, 15), Some(KeyCode::Esc));
    }

    #[test]
    fn the_playback_buttons_sit_at_the_right_of_the_status_line() {
        let status = panes(Rect::new(0, 0, 100, 20)).status;
        // "␣ pause · ← −5s · → +5s · , −1s · . +1s" is 39 columns.
        let left = playback_left(status);
        assert_eq!(left, 61);
        assert_eq!(bar_hit(PLAYBACK, left, 61), Some(KeyCode::Char(' ')));
        assert_eq!(bar_hit(PLAYBACK, left, 71), Some(KeyCode::Left));
        assert_eq!(bar_hit(PLAYBACK, left, 99), Some(KeyCode::Char('.')));
    }

    #[test]
    fn the_playback_buttons_give_way_to_a_long_message_and_a_narrow_screen() {
        // 80 - 39 leaves 41 columns, one of them clear of the buttons.
        let status = Rect::new(0, 18, 80, 1);
        assert!(playback_shown(status, None));
        assert!(playback_shown(status, Some(&"x".repeat(40))));
        assert!(!playback_shown(status, Some(&"x".repeat(41))));
        assert!(playback_shown(Rect::new(0, 18, 40, 1), None));
        assert!(!playback_shown(Rect::new(0, 18, 39, 1), None));
    }

    #[test]
    fn a_narrow_key_bar_wraps_between_items() {
        let rows = |w| {
            bar_rows(KEYS, w)
                .into_iter()
                .map(bar_text)
                .collect::<Vec<_>>()
        };
        assert_eq!(rows(105), [bar_text(KEYS)]);
        assert_eq!(rows(104).len(), 2);
        assert_eq!(
            rows(80),
            [
                "j/k move · o mpv · a accept · r reject · e edit · u undo · ⏎ expand · f filter",
                "n next · p prev · q quit"
            ]
        );
        assert_eq!(rows(3).len(), KEYS.len(), "an item to a row");
        // The panes give up a row to it.
        let p = panes(Rect::new(0, 0, 80, 20));
        assert_eq!(p.list.height, 17);
        assert_eq!(p.status, Rect::new(0, 17, 80, 1));
        assert_eq!(p.keys, Rect::new(0, 18, 80, 2));
        // A click presses the button drawn under it, on either row.
        let at = |x, y| bar_click(KEYS, p.keys, Position::new(x, y));
        assert_eq!(at(11, 18), Some(KeyCode::Char('o')));
        assert_eq!(at(79, 18), None, "past the first row's end");
        assert_eq!(at(0, 19), Some(KeyCode::Char('n')));
        assert_eq!(at(18, 19), Some(KeyCode::Char('q')));
        assert_eq!(at(0, 17), None, "the status line");
    }

    #[test]
    fn the_list_scrolls_only_to_keep_the_selection_in_view() {
        // 6 rows from 3 of 40: entries 3 to 8 show.
        assert_eq!(keep_in_view(3, 5, 6, 40), 3);
        assert_eq!(keep_in_view(3, 8, 6, 40), 3);
        assert_eq!(keep_in_view(3, 9, 6, 40), 4, "one past the bottom");
        assert_eq!(keep_in_view(3, 2, 6, 40), 2, "one above the top");
        assert_eq!(keep_in_view(0, 30, 6, 40), 25);
        assert_eq!(keep_in_view(3, 9, 0, 40), 3, "no rows");
    }

    #[test]
    fn the_list_never_scrolls_past_its_last_full_screen() {
        // 12 entries from 9 would show 9 to 11 over 3 blank rows.
        assert_eq!(keep_in_view(9, 11, 6, 12), 6);
        assert_eq!(keep_in_view(9, 9, 6, 10), 4, "a filter left 10");
        assert_eq!(keep_in_view(6, 11, 16, 12), 0, "more rows than entries");
        assert_eq!(keep_in_view(6, 0, 16, 0), 0, "an empty list");
    }
}
