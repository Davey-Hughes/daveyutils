//! Pure render of the review `Model` into a ratatui frame.

use std::fmt::Write;
use std::ops::Range;

use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use super::model::{changed, Entry, FileNote, Model};
use crate::list::{self, label, Filter};
use crate::store::State;
use crate::text::timestamp;

const KEYS: &str =
    "o mpv · a accept · r reject · e edit · u undo · ⏎ expand · f filter · n/p file · q quit";

pub fn view(m: &Model, f: &mut Frame) {
    let [body, footer] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .areas(f.area());
    let [left, right] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .areas(body);
    list_pane(m, f, left);
    detail_pane(m, f, right);
    footer_line(m, f, footer);
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
    let mut state = ListState::default().with_selected(Some(m.selected));
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

    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
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

fn footer_line(m: &Model, f: &mut Frame, area: Rect) {
    if let Some(ed) = &m.editor {
        let prompt = "wording (⏎ accept · Esc cancel): ";
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
    let text = m.status.clone().unwrap_or_else(|| KEYS.to_string());
    f.render_widget(Paragraph::new(text), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

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
    fn a_changed_file_and_a_status_show() {
        let mut m = sample();
        m.notes
            .insert(m.run.findings[0].file.clone(), FileNote::Changed);
        m.status = Some("not saved: disk full".into());
        let screen = render(&m, 160, 20);
        assert!(screen.contains("⚠ file changed since scan"), "{screen}");
        assert!(
            screen
                .lines()
                .last()
                .unwrap()
                .starts_with("not saved: disk full"),
            "{screen}"
        );
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
        let mut term = Terminal::new(TestBackend::new(50, 10)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        let footer: String = (0..50)
            .map(|x| term.backend().buffer()[(x, 9)].symbol().to_string())
            .collect();
        // The 33-column prompt leaves 17: the text's last 16 characters, then the cursor.
        assert!(footer.ends_with("4567890123456789 "), "{footer}");
        assert_eq!(term.get_cursor_position().unwrap(), Position::new(49, 9));
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
    fn the_editor_takes_the_footer_and_the_cursor() {
        let mut m = sample();
        m.editor = Some(super::super::model::Editor {
            entry: Entry::Row(0),
            text: "abc".into(),
            cursor: 1,
        });
        let mut term = Terminal::new(TestBackend::new(160, 20)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        let pos = term.get_cursor_position().unwrap();
        // The prompt is 33 characters, then one character of text before the cursor.
        assert_eq!((pos.x, pos.y), (34, 19));
    }
}
