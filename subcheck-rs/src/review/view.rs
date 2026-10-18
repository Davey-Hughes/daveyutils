//! Pure render of the review `Model` into a ratatui frame.

use std::fmt::Write;
use std::ops::Range;

use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::layout::{self, EDIT_KEYS, KEYS, PLAYBACK};
use super::model::{changed, Entry, FileNote, Model};
use crate::list::{self, label, Filter};
use crate::store::State;
use crate::text::timestamp;

pub fn view(m: &Model, f: &mut Frame) {
    let p = layout::panes(f.area());
    list_pane(m, f, p.list);
    detail_pane(m, f, p.detail);
    status_line(m, f, p.status);
    let keys = if m.editor.is_some() { EDIT_KEYS } else { KEYS };
    let rows: Vec<Line> = layout::bar_rows(keys, p.keys.width)
        .into_iter()
        .map(|row| Line::raw(layout::bar_text(row)))
        .collect();
    f.render_widget(Paragraph::new(rows), p.keys);
}

fn list_pane(m: &Model, f: &mut Frame, area: Rect) {
    let (pending, accepted, rejected) = m.counts();
    let showing = match m.filter {
        Filter::All => "all".to_string(),
        Filter::Only(state) => label(state).to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(format!(
        " findings ({pending} pending · {accepted} accepted · {rejected} rejected) · showing {showing} "
    ));
    let entries = m.entries();
    if entries.is_empty() {
        let hint = match m.filter {
            Filter::All => "No findings.".to_string(),
            Filter::Only(state) => format!("Nothing {} · f shows the others", label(state)),
        };
        f.render_widget(Paragraph::new(hint).block(block), area);
        return;
    }
    let items: Vec<ListItem> = entries
        .iter()
        .map(|&e| ListItem::new(entry_line(m, e)))
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_symbol("▶ ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default()
        .with_selected(Some(m.selected))
        .with_offset(m.list_scroll(f.area()));
    f.render_stateful_widget(list, area, &mut state);
}

fn mark(state: Option<State>) -> &'static str {
    match state {
        Some(State::Pending) => "·",
        Some(State::Accepted) => "✓",
        Some(State::Rejected) => "✗",
        None => "±",
    }
}

fn entry_line(m: &Model, e: Entry) -> Line<'static> {
    match e {
        Entry::Row(r) => {
            // What the row's keys act on: the members the filter shows.
            let all = m.rows[r].members.len();
            let shown = m.shown_members(r);
            let first = shown[0];
            let who = match (all, shown.len()) {
                (1, _) => m.label(&m.finding(first).file),
                (all, n) if n == all => format!("×{all}"),
                (all, n) => format!("×{n}/{all}"),
            };
            let f = m.finding(first);
            let wording = m.wording(first);
            let (ra, rb) = changed(&f.original, wording);
            Line::from(format!(
                "{} {who} {} {} → {}  [{}]",
                mark(m.shown_state(r)),
                timestamp(m.start_ms(first)),
                &f.original[ra],
                &wording[rb],
                f.style
            ))
        }
        Entry::Member(_, i) => Line::from(format!(
            "    {} {} {}",
            mark(Some(m.state(i))),
            m.label(&m.finding(i).file),
            timestamp(m.start_ms(i))
        )),
    }
}

fn detail_pane(m: &Model, f: &mut Frame, area: Rect) {
    let Some(i) = m.focus() else {
        f.render_widget(Block::default().borders(Borders::ALL), area);
        return;
    };
    let finding = m.finding(i);
    let name = finding.file.rsplit('/').next().unwrap_or(&finding.file);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {name} "));
    f.render_widget(
        Paragraph::new(detail_lines(m))
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((m.detail_scroll, 0)),
        area,
    );
}

/// The detail pane's lines on screen at `width` columns, wrapped as drawn: `update` stops the
/// wheel at the last of them.
pub fn detail_height(m: &Model, width: u16) -> usize {
    Paragraph::new(detail_lines(m))
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// What the detail pane says about the selection; nothing when there is none.
pub fn detail_lines(m: &Model) -> Vec<Line<'static>> {
    let Some(i) = m.focus() else {
        return Vec::new();
    };
    let finding = m.finding(i);
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Line> = Vec::new();

    if let Some(Entry::Row(r)) = m.selected_entry() {
        let all = m.rows[r].members.len();
        let shown = m.shown_members(r);
        if all > 1 {
            let episodes: Vec<String> =
                shown.iter().map(|&j| m.label(&m.finding(j).file)).collect();
            let hidden = match all - shown.len() {
                0 => String::new(),
                n => format!(" (+{n} hidden by the filter)"),
            };
            lines.push(Line::from(format!(
                "{} files: {}{hidden}",
                shown.len(),
                episodes.join(" ")
            )));
        }
    }
    let (occurrence, track) = list::shown_at(&m.run, finding);
    let mut at = occurrence.map_or_else(String::new, |o| timestamp(o.start_ms));
    if let Some(t) = track {
        let _ = write!(
            at,
            "  {} [track {} · sid {}]",
            t.name.as_deref().unwrap_or("(unnamed)"),
            t.id,
            t.sid
        );
    }
    if finding.occurrences.len() > 1 {
        let _ = write!(at, "  (+{} more)", finding.occurrences.len() - 1);
    }
    lines.push(Line::from(at));
    match m.notes.get(&finding.file) {
        Some(FileNote::Changed) => lines.push(warn("file changed since scan")),
        Some(FileNote::Missing) => lines.push(warn("file missing")),
        None => {}
    }
    let record = m.run.files.iter().rev().find(|r| r.path == finding.file);
    if let Some(t) = record.and_then(|r| r.truncated) {
        lines.push(warn(&format!("scanned a partial copy: {}", t.shortfall())));
    }
    let state = m.state(i);
    let edited = m
        .run
        .decisions
        .get(&finding.id)
        .and_then(|d| d.edited.as_ref());
    lines.push(Line::from(format!(
        "state: {}{}",
        label(state),
        if edited.is_some() {
            " (your wording)"
        } else {
            ""
        }
    )));
    lines.push(Line::default());

    let wording = m.wording(i);
    let (ra, rb) = changed(&finding.original, wording);
    lines.push(marked("- ", &finding.original, ra, Color::Red));
    lines.push(marked("+ ", wording, rb, Color::Green));
    if !finding.reason.is_empty() {
        lines.push(Line::from(format!("why: {}", finding.reason)));
    }
    lines.push(Line::default());
    lines.extend(finding.before.iter().map(|l| Line::styled(l.clone(), dim)));
    lines.push(Line::styled(
        finding.original.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    lines.extend(finding.after.iter().map(|l| Line::styled(l.clone(), dim)));
    lines
}

fn warn(text: &str) -> Line<'static> {
    Line::styled(
        format!("⚠ {text}"),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
}

/// `prefix` then `text`, with `span` of it highlighted.
fn marked(prefix: &'static str, text: &str, span: Range<usize>, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::raw(prefix),
        Span::raw(text[..span.start].to_string()),
        Span::styled(
            text[span.clone()].to_string(),
            Style::default()
                .fg(color)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ),
        Span::raw(text[span.end..].to_string()),
    ])
}

/// The status line with the playback buttons on its right, or the editor while it is open (the
/// key bar under it says how to leave).
fn status_line(m: &Model, f: &mut Frame, area: Rect) {
    if let Some(ed) = &m.editor {
        let prompt = "wording: ";
        let prompt_len = prompt.chars().count();
        // Scrolled so the cursor stays in view, with a column left for it after the text.
        let room = usize::from(area.width).saturating_sub(prompt_len).max(1);
        let start = ed.cursor.saturating_sub(room - 1);
        let shown: String = ed.text.chars().skip(start).take(room).collect();
        f.render_widget(Paragraph::new(format!("{prompt}{shown}")), area);
        let x = area.x + u16::try_from(prompt_len + ed.cursor - start).unwrap_or(u16::MAX);
        f.set_cursor_position(Position::new(x.min(area.right().saturating_sub(1)), area.y));
        return;
    }
    if !layout::playback_shown(area, m.status.as_deref()) {
        if let Some(status) = &m.status {
            f.render_widget(Paragraph::new(status.as_str()), area);
        }
        return;
    }
    let left = layout::playback_left(area);
    let buttons = Rect {
        x: left,
        width: area.right() - left,
        ..area
    };
    if let Some(status) = &m.status {
        // A column clear of the buttons, so a message does not run into them.
        let text = Rect {
            width: (left - area.x).saturating_sub(1),
            ..area
        };
        f.render_widget(Paragraph::new(status.as_str()), text);
    }
    f.render_widget(Paragraph::new(layout::bar_text(PLAYBACK)), buttons);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use crate::review::update::{self, Msg};

    fn now() -> jiff::Timestamp {
        "2026-10-17T12:00:00Z".parse().unwrap()
    }

    use crate::review::model::tests_support::sample;

    fn render(m: &Model, width: u16, height: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| view(m, f)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_list_groups_repeats_and_counts_findings() {
        let m = sample();
        let screen = render(&m, 160, 20);
        assert!(
            screen.contains("findings (4 pending · 0 accepted · 0 rejected) · showing pending"),
            "{screen}"
        );
        assert!(
            screen.contains("▶ · ×3 0:02:45.06 trough → through  [OP en]"),
            "{screen}"
        );
        assert!(
            screen.contains("· E02 0:14:54.19 everyday. → every day.  [Dialogue]"),
            "{screen}"
        );
    }

    #[test]
    fn the_detail_pane_shows_the_group_and_the_line() {
        let m = sample();
        let screen = render(&m, 160, 20);
        assert!(screen.contains("3 files: E01 E02 E03"), "{screen}");
        assert!(
            screen.contains("0:02:45.06  English Full [track 5 · sid 1]"),
            "{screen}"
        );
        assert!(
            screen.contains("- The beat gets trough to our mind"),
            "{screen}"
        );
        assert!(
            screen.contains("+ The beat gets through to our mind"),
            "{screen}"
        );
        assert!(
            screen.contains("why: misspelling of \"through\""),
            "{screen}"
        );
        assert!(screen.contains("state: pending"), "{screen}");
    }

    #[test]
    fn a_changed_file_and_a_status_show_above_the_key_bar() {
        let mut m = sample();
        m.notes
            .insert(m.run.findings[0].file.clone(), FileNote::Changed);
        m.status = Some("not saved: disk full".into());
        let screen = render(&m, 160, 20);
        assert!(screen.contains("⚠ file changed since scan"), "{screen}");
        let lines: Vec<&str> = screen.lines().collect();
        assert!(lines[18].starts_with("not saved: disk full"), "{screen}");
        assert!(
            lines[19].starts_with("j/k move · o mpv · a accept · r reject · e edit · u undo"),
            "{screen}"
        );
    }

    #[test]
    fn the_detail_pane_scrolls() {
        let mut m = sample();
        m.detail_scroll = 2;
        let screen = render(&m, 160, 20);
        assert!(!screen.contains("3 files:"), "{screen}");
        assert!(
            screen.lines().nth(1).unwrap().contains("│state: pending"),
            "{screen}"
        );
    }

    #[test]
    fn the_wheel_reaches_the_end_of_a_wrapped_detail() {
        let mut m = sample();
        // 8 lines leave the detail pane 4 rows of 28 columns, so its lines wrap.
        update::update(&mut m, Msg::Resize(60, 8), now());
        for _ in 0..20 {
            update::update(&mut m, Msg::Mouse(update::Mouse::WheelDown, 40, 2), now());
        }
        let screen = render(&m, 60, 8);
        // It stops with the last line on top.
        assert!(
            screen.lines().nth(1).unwrap().contains("│The line after."),
            "{screen}"
        );
    }

    #[test]
    fn a_wider_screen_scrolls_a_detail_back_from_past_its_end() {
        let mut m = sample();
        update::update(&mut m, Msg::Resize(60, 8), now());
        for _ in 0..20 {
            update::update(&mut m, Msg::Mouse(update::Mouse::WheelDown, 40, 2), now());
        }
        // Wider, the lines wrap less: the last one is on top again, not off the end.
        update::update(&mut m, Msg::Resize(160, 8), now());
        let screen = render(&m, 160, 8);
        assert!(
            screen.lines().nth(1).unwrap().contains("│The line after."),
            "{screen}"
        );
    }

    #[test]
    fn the_playback_buttons_share_the_status_line() {
        let mut m = sample();
        m.status = Some("⏸ 0:02:51.06".into());
        let screen = render(&m, 110, 20);
        let line = screen.lines().nth(18).unwrap();
        assert!(line.starts_with("⏸ 0:02:51.06 "), "{screen}");
        assert!(
            line.ends_with(" ␣ pause · ← −5s · → +5s · , −1s · . +1s"),
            "{screen}"
        );
        // A message that fits stops a column short of the buttons.
        m.status = Some("x".repeat(70));
        let screen = render(&m, 110, 20);
        let line = screen.lines().nth(18).unwrap();
        assert!(
            line.starts_with(&format!("{} ␣ pause", "x".repeat(70))),
            "{screen}"
        );
        // One too long for that takes the whole line, and the buttons give way.
        m.status = Some(format!("{}y", "x".repeat(109)));
        let screen = render(&m, 110, 20);
        assert_eq!(
            screen.lines().nth(18).unwrap(),
            format!("{}y", "x".repeat(109))
        );
    }

    #[test]
    fn a_narrow_screen_wraps_the_key_bar() {
        let screen = render(&sample(), 80, 20);
        let lines: Vec<&str> = screen.lines().collect();
        assert!(
            lines[17].ends_with(" ␣ pause · ← −5s · → +5s · , −1s · . +1s"),
            "{screen}"
        );
        assert!(lines[18].starts_with("j/k move · o mpv"), "{screen}");
        assert!(lines[18].trim_end().ends_with("· f filter"), "{screen}");
        assert!(
            lines[19].starts_with("n next · p prev · q quit"),
            "{screen}"
        );
    }

    #[test]
    fn the_list_stays_put_while_the_selection_moves_inside_it() {
        let mut m = crate::review::model::tests_support::many(12);
        // 10 lines: 2 for the footer and 2 for the border leave 6 rows.
        update::update(&mut m, Msg::Resize(110, 10), now());
        for _ in 0..8 {
            update::update(&mut m, Msg::Key(KeyCode::Char('j')), now());
        }
        for _ in 0..2 {
            update::update(&mut m, Msg::Key(KeyCode::Char('k')), now());
        }
        assert_eq!((m.selected, m.scroll), (6, 3));
        let screen = render(&m, 110, 10);
        let lines: Vec<&str> = screen.lines().collect();
        // Entries 3 to 8 show, E04 to E09, and the selection is on E07 in the middle.
        assert!(lines[1].contains("E04 "), "{screen}");
        assert!(lines[4].contains("▶ · E07 "), "{screen}");
        assert!(lines[6].contains("E09 "), "{screen}");
    }

    #[test]
    fn a_taller_screen_shows_the_entries_above_instead_of_blank_rows() {
        let mut m = crate::review::model::tests_support::many(12);
        update::update(&mut m, Msg::Resize(110, 10), now());
        update::update(&mut m, Msg::Key(KeyCode::End), now());
        assert_eq!(m.scroll, 6);
        // 20 lines leave 16 rows: all 12 entries fit, from the top.
        update::update(&mut m, Msg::Resize(110, 20), now());
        assert_eq!(m.scroll, 0);
        let screen = render(&m, 110, 20);
        let lines: Vec<&str> = screen.lines().collect();
        assert!(lines[1].contains("E01 "), "{screen}");
        assert!(lines[12].contains("▶ · E12 "), "{screen}");
    }

    #[test]
    fn a_partly_decided_group_shows_what_its_keys_act_on() {
        let mut m = sample();
        m.run.decisions.insert(
            1,
            crate::store::Decision {
                finding: 1,
                state: State::Rejected,
                edited: None,
                at: jiff::Timestamp::UNIX_EPOCH,
            },
        );
        let screen = render(&m, 160, 20);
        assert!(
            screen.contains("▶ · ×2/3 0:02:45.06 trough → through  [OP en]"),
            "{screen}"
        );
        assert!(
            screen.contains("2 files: E02 E03 (+1 hidden by the filter)"),
            "{screen}"
        );
    }

    #[test]
    fn a_long_edit_scrolls_to_keep_the_cursor_in_view() {
        let mut m = sample();
        m.editor = Some(super::super::model::Editor {
            entry: Entry::Row(0),
            text: "0123456789".repeat(4),
            cursor: 40,
        });
        let mut term = Terminal::new(TestBackend::new(30, 10)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        // 30 columns fit the key bar in 4 rows, and it keeps them while editing, so that the
        // panes stay put: the status line is y 5.
        let status: String = (0..30)
            .map(|x| term.backend().buffer()[(x, 5)].symbol().to_string())
            .collect();
        // The 9-column prompt leaves 21: the text's last 20 characters, then the cursor.
        assert_eq!(status, "wording: 01234567890123456789 ");
        assert_eq!(term.get_cursor_position().unwrap(), Position::new(29, 5));
    }

    #[test]
    fn an_empty_filter_says_how_to_see_the_rest() {
        let mut m = sample();
        m.filter = Filter::Only(State::Rejected);
        let screen = render(&m, 160, 20);
        assert!(
            screen.contains("Nothing rejected · f shows the others"),
            "{screen}"
        );
    }

    #[test]
    fn the_editor_takes_the_status_line_and_its_keys_the_bar() {
        let mut m = sample();
        m.editor = Some(super::super::model::Editor {
            entry: Entry::Row(0),
            text: "abc".into(),
            cursor: 1,
        });
        let mut term = Terminal::new(TestBackend::new(160, 20)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        let pos = term.get_cursor_position().unwrap();
        // The prompt is 9 characters, then one character of text before the cursor.
        assert_eq!((pos.x, pos.y), (10, 18));
        let screen = render(&m, 160, 20);
        assert!(
            screen.lines().nth(18).unwrap().starts_with("wording: abc"),
            "{screen}"
        );
        assert!(
            screen
                .lines()
                .nth(19)
                .unwrap()
                .starts_with("⏎ accept · Esc cancel "),
            "{screen}"
        );
    }
}
