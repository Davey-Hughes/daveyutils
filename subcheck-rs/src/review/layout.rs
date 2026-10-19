//! Where each part of the screen is, and which key each button presses. Pure, so the view draws
//! with the same numbers that `update` maps a click with.

use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::text::Span;

/// One button of a bar: its text, and the key a click presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    pub text: &'static str,
    pub key: KeyCode,
}

const fn button(text: &'static str, key: KeyCode) -> Item {
    Item { text, key }
}

/// At the end of the status line: what `?` shows, every key.
pub const HELP: &[Item] = &[button("? help", KeyCode::Char('?'))];

/// At the end of the status line while the editor is open.
pub const EDIT_KEYS: &[Item] = &[
    button("⏎ accept", KeyCode::Enter),
    button("Esc cancel", KeyCode::Esc),
];

/// The editor's prompt, on the status line.
pub const PROMPT: &str = "wording: ";

/// Columns of text the editor keeps beside its prompt before its buttons show.
pub const EDIT_FIELD: u16 = 20;

const SEP: &str = " · ";

/// The parts of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    pub list: Rect,
    pub detail: Rect,
    /// The last line: messages, or the editor while it is open, with its buttons at the end.
    pub status: Rect,
}

pub fn panes(area: Rect) -> Panes {
    let [body, status] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .areas(area);
    let [list, detail] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .areas(body);
    Panes {
        list,
        detail,
        status,
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

/// The key a click at column `x` presses, on a bar drawn from column `left`.
pub fn bar_hit(items: &[Item], left: u16, x: u16) -> Option<KeyCode> {
    let mut at = left;
    for (n, item) in items.iter().enumerate() {
        if n > 0 {
            at = at.saturating_add(width(SEP));
        }
        let end = at.saturating_add(width(item.text));
        if (at..end).contains(&x) {
            return Some(item.key);
        }
        at = end;
    }
    None
}

/// Where `items` start at the end of the status line `status`: flush right.
pub fn bar_left(status: Rect, items: &[Item]) -> u16 {
    status
        .right()
        .saturating_sub(width(&bar_text(items)))
        .max(status.x)
}

/// Whether `items` show at the end of the status line `status` beside `left` columns of text:
/// they give way to text too long to fit to their left with a column clear, and to a screen no
/// wider than they are.
pub fn right_shown(status: Rect, items: &[Item], left: u16) -> bool {
    let room = (bar_left(status, items) - status.x).saturating_sub(1);
    status.width > width(&bar_text(items)) && left <= room
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
    fn the_footer_is_one_line_under_the_panes() {
        let p = panes(Rect::new(0, 0, 110, 20));
        assert_eq!(p.list, Rect::new(0, 0, 55, 19));
        assert_eq!(p.detail, Rect::new(55, 0, 55, 19));
        assert_eq!(p.status, Rect::new(0, 19, 110, 1));
        assert_eq!(list_rows(&p), Rect::new(1, 1, 53, 17));
    }

    #[test]
    fn buttons_sit_at_the_right_end_of_the_status_line() {
        let status = panes(Rect::new(0, 0, 110, 20)).status;
        // `? help` is the last 6 columns.
        let left = bar_left(status, HELP);
        assert_eq!(left, 104);
        assert_eq!(bar_hit(HELP, left, 103), None);
        assert_eq!(bar_hit(HELP, left, 104), Some(KeyCode::Char('?')));
        assert_eq!(bar_hit(HELP, left, 109), Some(KeyCode::Char('?')));
        // "⏎ accept · Esc cancel" is the last 21: `⏎` is one column wide, as drawn.
        let left = bar_left(status, EDIT_KEYS);
        assert_eq!(left, 89);
        assert_eq!(bar_hit(EDIT_KEYS, left, 89), Some(KeyCode::Enter));
        assert_eq!(bar_hit(EDIT_KEYS, left, 97), None, "the separator");
        assert_eq!(bar_hit(EDIT_KEYS, left, 100), Some(KeyCode::Esc));
        assert_eq!(bar_hit(EDIT_KEYS, left, 109), Some(KeyCode::Esc));
    }

    #[test]
    fn buttons_give_way_to_a_long_message_and_a_narrow_screen() {
        // 110 - 6 leaves 104 columns, one of them clear of `? help`.
        let status = Rect::new(0, 19, 110, 1);
        assert!(right_shown(status, HELP, 0));
        assert!(right_shown(status, HELP, 103));
        assert!(!right_shown(status, HELP, 104));
        assert!(right_shown(Rect::new(0, 19, 7, 1), HELP, 0));
        assert!(!right_shown(Rect::new(0, 19, 6, 1), HELP, 0));
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
