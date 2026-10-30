//! Pure render of the review `Model` into a ratatui frame.

use std::fmt::Write;
use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};

use super::files;
use super::layout;
use super::model::{Entry, FileNote, Model, Pane, changed, fix_label};
use crate::list::{self, Filter, label};
use crate::llm::Spent;
use crate::store::{State, TimingFix};
use crate::text::{timestamp, timestamp_ms};
use crate::timing::shift_label;

pub fn view(m: &Model, f: &mut Frame) {
    let p = layout::panes(f.area());
    match m.pane {
        Pane::Findings => list_pane(m, f, p.list),
        Pane::Files => files::files_pane(m, f, p.list),
    }
    detail_pane(m, f, p.detail);
    status_line(m, f, p.status);
    if m.help {
        keys_box(m, f, f.area());
    }
}

/// What `?` shows, a column to each group: each key and what it does.
const KEY_GROUPS: [(&str, &[(&str, &str)]); 3] = [
    (
        "decide",
        &[
            ("a", "accept"),
            ("r", "reject"),
            ("x", "clear: back to pending"),
            ("e", "reword, then accept"),
            ("u", "undo"),
        ],
    ),
    (
        "move",
        &[
            ("j k ↑ ↓", "move"),
            ("g G", "top, bottom"),
            ("PgUp PgDn", "page"),
            ("n p", "next, previous file"),
            ("⏎", "expand a group"),
            ("f", "filter"),
            ("Tab", "findings, files"),
        ],
    ),
    (
        "mpv",
        &[
            ("o", "open the line"),
            ("␣", "pause"),
            ("← →", "5 s back, forward"),
            (", .", "1 s back, forward"),
            ("[ ]", "a frame back, forward"),
        ],
    ),
];

/// [`KEY_GROUPS`] as lines: as many groups side by side as fit in `columns`, the rest under them.
fn keys_lines(columns: usize) -> Vec<Line<'static>> {
    const GAP: usize = 4;
    let w = |s: &str| usize::from(layout::width(s));
    let pad = |s: &str, to: usize| format!("{s}{}", " ".repeat(to.saturating_sub(w(s))));
    // Each group: its heading, its rows, and the width of the widest.
    let groups: Vec<(&str, Vec<String>, usize)> = KEY_GROUPS
        .iter()
        .map(|&(name, keys)| {
            let key_width = keys.iter().map(|&(k, _)| w(k)).max().unwrap_or(0);
            let rows: Vec<String> = keys
                .iter()
                .map(|&(k, what)| format!("{}  {what}", pad(k, key_width)))
                .collect();
            let width = rows
                .iter()
                .map(|r| w(r))
                .chain([w(name)])
                .max()
                .unwrap_or(0);
            (name, rows, width)
        })
        .collect();
    let mut bands: Vec<Vec<&(&str, Vec<String>, usize)>> = Vec::new();
    let mut used = 0;
    for g in &groups {
        match bands.last_mut() {
            Some(band) if used + GAP + g.2 <= columns => {
                band.push(g);
                used += GAP + g.2;
            }
            _ => {
                bands.push(vec![g]);
                used = g.2;
            }
        }
    }
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    for band in bands {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        let mut heading = Vec::new();
        for (n, g) in band.iter().enumerate() {
            if n > 0 {
                heading.push(Span::raw(" ".repeat(GAP)));
            }
            heading.push(Span::styled(pad(g.0, g.2), bold));
        }
        lines.push(Line::from(heading));
        let height = band.iter().map(|g| g.1.len()).max().unwrap_or(0);
        for r in 0..height {
            let row: Vec<String> = band
                .iter()
                .map(|g| pad(g.1.get(r).map_or("", String::as_str), g.2))
                .collect();
            lines.push(Line::raw(row.join(&" ".repeat(GAP)).trim_end().to_string()));
        }
    }
    lines
}

/// Under the keys, what the run's LLM calls cost, when it was recorded: its parts on as few lines
/// of `columns` as they fit.
fn spent_lines(spent: &Spent, columns: usize) -> Vec<Line<'static>> {
    const SEP: &str = " · ";
    let parts = spent.parts();
    if parts.is_empty() {
        return Vec::new();
    }
    let w = |s: &str| usize::from(layout::width(s));
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::raw(""), Line::styled("LLM", bold)];
    let mut line = String::new();
    for part in parts {
        if !line.is_empty() && w(&line) + w(SEP) + w(&part) > columns {
            lines.push(Line::raw(std::mem::take(&mut line)));
        }
        if !line.is_empty() {
            line.push_str(SEP);
        }
        line.push_str(&part);
    }
    lines.push(Line::raw(line));
    lines
}

/// The box of keys `?` opens, over the middle of `area`, with how to leave it in its bottom
/// border: on a screen too short for every key that still shows.
fn keys_box(m: &Model, f: &mut Frame, area: Rect) {
    const LEAVE: &str = " q quits · any other key closes this ";
    // Inside a border and a column of padding each side.
    let columns = usize::from(area.width.saturating_sub(4));
    let mut lines = keys_lines(columns);
    lines.extend(spent_lines(&m.run.spent, columns));
    let widest = lines.iter().map(Line::width).max().unwrap_or(0);
    let widest = widest.max(usize::from(layout::width(LEAVE)));
    let width = u16::try_from(widest + 4)
        .unwrap_or(u16::MAX)
        .min(area.width);
    let height = u16::try_from(lines.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height);
    let at = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" keys ")
        .title_bottom(LEAVE)
        .padding(Padding::horizontal(1));
    f.render_widget(Clear, at);
    f.render_widget(Paragraph::new(lines).block(block), at);
}

/// The two tab names that start the left pane's title, the one for `active` reversed. They sit in
/// the same place in both views, so the title says which view is up before anything else does. The
/// other one is only dimmed: it should read as a place to go, not as a gap.
pub(super) fn tabs(active: Pane) -> Vec<Span<'static>> {
    let tab = |pane, name: &'static str| {
        let style = if pane == active {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        Span::styled(name, style)
    };
    vec![
        Span::raw(" "),
        tab(Pane::Findings, "findings"),
        Span::raw("  "),
        tab(Pane::Files, "files"),
    ]
}

fn list_pane(m: &Model, f: &mut Frame, area: Rect) {
    let (pending, accepted, rejected) = m.counts();
    let showing = match m.filter {
        Filter::All => "all".to_string(),
        Filter::Only(state) => label(state).to_string(),
    };
    let progress = m
        .progress
        .title()
        .map_or_else(String::new, |t| format!(" · {t}"));
    let mut title = tabs(Pane::Findings);
    title.push(Span::raw(format!(
        " · {pending} pending · {accepted} accepted · {rejected} rejected · showing {showing}{progress} "
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(title));
    let entries = m.entries();
    if entries.is_empty() {
        let hint = match m.filter {
            Filter::All if m.progress.running() => {
                "No findings yet · Tab shows the files".to_string()
            }
            Filter::All => "No findings · Tab shows the files".to_string(),
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

/// A list line's colour by its state: accepted green, rejected red, mixed yellow, pending the
/// terminal's own.
fn colour(state: Option<State>) -> Style {
    let fg = match state {
        Some(State::Pending) => return Style::default(),
        Some(State::Accepted) => Color::Green,
        Some(State::Rejected) => Color::Red,
        None => Color::Yellow,
    };
    Style::default().fg(fg)
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
    // A row with anything pending is still to do, however its decided members differ.
    let state = match e {
        Entry::Row(_) if m.has_pending(e) => Some(State::Pending),
        Entry::Row(r) => m.shown_state(r),
        Entry::Member(_, i) => Some(m.state(i)),
    };
    entry_text(m, e).style(colour(state))
}

fn entry_text(m: &Model, e: Entry) -> Line<'static> {
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
            if f.timing.is_some() {
                return Line::from(format!(
                    "{} {who} {}  {}",
                    mark(m.shown_state(r)),
                    m.track_name(first),
                    m.fix_summary(&shown)
                ));
            }
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
        Entry::Member(_, i) => {
            // A timing finding's members differ in their fix, not in a time.
            let what = if m.finding(i).timing.is_some() {
                fix_label(m.finding(i))
            } else {
                timestamp(m.start_ms(i))
            };
            Line::from(format!(
                "    {} {} {what}",
                mark(Some(m.state(i))),
                m.label(&m.finding(i).file)
            ))
        }
    }
}

fn detail_pane(m: &Model, f: &mut Frame, area: Rect) {
    let path = match m.pane {
        Pane::Findings => m.focus().map(|i| m.finding(i).file.as_str()),
        Pane::Files => m
            .progress
            .files
            .get(m.file_selected)
            .map(|file| file.path.as_str()),
    };
    let Some(path) = path else {
        f.render_widget(Block::default().borders(Borders::ALL), area);
        return;
    };
    let name = path.rsplit('/').next().unwrap_or(path);
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
    if m.pane == Pane::Files {
        return files::detail_lines(m);
    }
    let Some(i) = m.focus() else {
        return Vec::new();
    };
    let finding = m.finding(i);
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Line> = Vec::new();

    lines.extend(group_line(m));
    let (occurrence, track) = match &finding.timing {
        // The whole track, not one line of it.
        Some(t) => (None, list::track_of(&m.run, finding, t.track)),
        None => list::shown_at(&m.run, finding),
    };
    let mut at = occurrence.map_or_else(String::new, |o| timestamp(o.start_ms));
    if let Some(t) = track {
        let _ = write!(at, "  {} [track {} · sid {}]", t.shown_name(), t.id, t.sid);
    }
    if finding.occurrences.len() > 1 && finding.timing.is_none() {
        let _ = write!(at, "  (+{} more)", finding.occurrences.len() - 1);
    }
    lines.push(Line::from(at.trim_start().to_string()));
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
    let (ra, rb) = match finding.timing {
        Some(_) => (0..finding.original.len(), 0..wording.len()),
        None => changed(&finding.original, wording),
    };
    lines.push(marked("- ", &finding.original, ra, Color::Red));
    lines.push(marked("+ ", wording, rb, Color::Green));
    if !finding.reason.is_empty() {
        lines.push(Line::from(format!("why: {}", finding.reason)));
    }
    lines.push(Line::default());
    if let Some(t) = &finding.timing {
        lines.extend(stretch_lines(t));
        lines.extend(opens_line(m, i));
        return lines;
    }
    lines.extend(finding.before.iter().map(|l| Line::styled(l.clone(), dim)));
    lines.push(Line::styled(
        finding.original.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    lines.extend(finding.after.iter().map(|l| Line::styled(l.clone(), dim)));
    lines
}

/// For a row of several findings, what they are: a typo's files, a timing row's members' fixes.
fn group_line(m: &Model) -> Option<Line<'static>> {
    let Some(Entry::Row(r)) = m.selected_entry() else {
        return None;
    };
    let all = m.rows[r].members.len();
    let shown = m.shown_members(r);
    if all < 2 {
        return None;
    }
    let hidden = match all - shown.len() {
        0 => String::new(),
        n => format!(" (+{n} hidden by the filter)"),
    };
    let each = |j: usize| m.label(&m.finding(j).file);
    let files = if m.finding(shown[0]).timing.is_some() {
        let fixes: Vec<String> = shown
            .iter()
            .map(|&j| format!("{} {}", each(j), fix_label(m.finding(j))))
            .collect();
        fixes.join(" · ")
    } else {
        let episodes: Vec<String> = shown.iter().map(|&j| each(j)).collect();
        format!("{} files: {}", shown.len(), episodes.join(" "))
    };
    Some(Line::from(format!("{files}{hidden}")))
}

/// Where `o` opens timing finding `i`.
fn opens_line(m: &Model, i: usize) -> Vec<Line<'static>> {
    let Some(at) = list::timing_moment(&m.run, m.finding(i)) else {
        return Vec::new();
    };
    let on = if at.on_frame {
        "the frame that shows the error"
    } else {
        "its first moved line (no line of it ends or starts on a cut)"
    };
    vec![
        Line::default(),
        Line::styled(
            format!("o opens {}, paused on {on}", timestamp_ms(at.at_ms)),
            Style::default().fg(Color::DarkGray),
        ),
    ]
}

/// A timing fix's stretches, each from its first line's start to its last line's end with its
/// shift, then the lines it leaves where they are.
fn stretch_lines(t: &TimingFix) -> Vec<Line<'static>> {
    let mut rows: Vec<(String, String, String)> = t
        .stretches
        .iter()
        .map(|s| {
            let from = s.from_ms.or(s.first.as_ref().map(|l| l.start_ms));
            let to = s.to_ms.or(s.last.as_ref().map(|l| l.end_ms));
            (
                shift_label(s.frames),
                from.map_or_else(|| "start".into(), timestamp),
                to.map_or_else(|| "end".into(), timestamp),
            )
        })
        .collect();
    rows.extend(t.unshifted.iter().map(|l| {
        (
            "not shifted".to_string(),
            timestamp(l.start_ms),
            timestamp(l.end_ms),
        )
    }));
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    let mut lines: Vec<Line> = rows
        .into_iter()
        .map(|(label, from, to)| Line::from(format!("{label:<width$}  {from} – {to}")))
        .collect();
    if !t.unshifted.is_empty() {
        let note = if t.unshifted.len() == 1 {
            "(the line not shifted: its times point both ways)"
        } else {
            "(lines not shifted: their times point both ways)"
        };
        lines.push(Line::styled(note, Style::default().fg(Color::DarkGray)));
    }
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

/// The status line: the message, or the editor while it is open, with its buttons at the end
/// while there is room (`? help`, or the editor's). A question waiting for `y` takes the line from
/// the editor too, since the next key answers it.
fn status_line(m: &Model, f: &mut Frame, area: Rect) {
    let buttons = m.buttons(area);
    // The text stops a column short of the buttons.
    let text = match buttons {
        Some(items) => Rect {
            width: (layout::bar_left(area, items) - area.x).saturating_sub(1),
            ..area
        },
        None => area,
    };
    if let Some(items) = buttons {
        let left = layout::bar_left(area, items);
        let at = Rect {
            x: left,
            width: area.right() - left,
            ..area
        };
        f.render_widget(Paragraph::new(layout::bar_text(items)), at);
    }
    if let Some(ed) = m.editor.as_ref().filter(|_| m.confirm.is_none()) {
        let prompt_len = layout::PROMPT.chars().count();
        // Scrolled so the cursor stays in view, with a column left for it after the text.
        let room = usize::from(text.width).saturating_sub(prompt_len).max(1);
        let start = ed.cursor.saturating_sub(room - 1);
        let shown: String = ed.text.chars().skip(start).take(room).collect();
        f.render_widget(Paragraph::new(format!("{}{shown}", layout::PROMPT)), text);
        let x = text.x + u16::try_from(prompt_len + ed.cursor - start).unwrap_or(u16::MAX);
        f.set_cursor_position(Position::new(x.min(text.right().saturating_sub(1)), text.y));
        return;
    }
    if let Some(status) = m.status_text(area) {
        f.render_widget(Paragraph::new(status), text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
    fn the_title_marks_the_tab_you_are_on() {
        let mut m = sample();
        // The cells of `name` on the top border: its modifier is what tells the tabs apart.
        let reversed = |m: &Model, name: &str| {
            let mut term = Terminal::new(TestBackend::new(200, 20)).unwrap();
            term.draw(|f| view(m, f)).unwrap();
            let buf = term.backend().buffer().clone();
            let row: Vec<&str> = (0..buf.area.width).map(|x| buf[(x, 0)].symbol()).collect();
            let at = (0..row.len())
                .find(|&i| row[i..].concat().starts_with(name))
                .unwrap();
            buf[(u16::try_from(at).unwrap(), 0)]
                .modifier
                .contains(Modifier::REVERSED)
        };
        assert!(reversed(&m, "findings"));
        assert!(!reversed(&m, "files"));
        update::update(&mut m, Msg::Key(KeyCode::Tab), now());
        assert!(!reversed(&m, "findings"));
        assert!(reversed(&m, "files"));
    }

    #[test]
    fn the_list_groups_repeats_and_counts_findings() {
        let m = sample();
        let screen = render(&m, 160, 20);
        assert!(
            screen.contains("findings  files · 4 pending · 0 accepted · 0 rejected · showing all"),
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
    fn a_timing_row_shows_its_track_and_its_members_fixes() {
        let mut m = crate::review::model::tests_support::with_timing();
        let screen = render(&m, 160, 20);
        assert!(
            screen.contains("· ×3 English Full  −1 frame ×2 · partial ×1"),
            "{screen}"
        );
        m.selected = 1;
        update::update(&mut m, Msg::Key(KeyCode::Enter), now());
        let screen = render(&m, 160, 20);
        assert!(screen.contains("    · E01 −1 frame"), "{screen}");
        assert!(screen.contains("    · E02 partial"), "{screen}");
    }

    #[test]
    fn the_detail_pane_shows_a_timing_rows_fixes_and_stretches() {
        let mut m = crate::review::model::tests_support::with_timing();
        m.selected = 1;
        let screen = render(&m, 160, 24);
        assert!(
            screen.contains("│E01 −1 frame · E02 partial · E03 −1 frame"),
            "{screen}"
        );
        assert!(
            screen.contains("│English Full [track 5 · sid 1]"),
            "{screen}"
        );
        assert!(screen.contains("│- 1 frame late"), "{screen}");
        assert!(screen.contains("│+ shift 1 frame earlier"), "{screen}");
        assert!(
            screen.contains("│why: cuts: 345 times, +1 in 5 of 9 windows"),
            "{screen}"
        );
        assert!(
            screen.contains("│1 frame late  0:00:01.00 – 0:23:22.00"),
            "{screen}"
        );
        assert!(
            screen.contains("│o opens 0:12:43.763, paused on the frame that shows the error"),
            "{screen}"
        );
        // E02, the partial one.
        update::update(&mut m, Msg::Key(KeyCode::Enter), now());
        m.selected = 3;
        let screen = render(&m, 160, 24);
        for line in [
            "│1 frame late  0:00:01.00 – 0:07:55.78",
            "│in sync       0:08:03.51 – 0:15:39.00",
            "│1 frame late  0:15:40.47 – 0:23:22.00",
            "│not shifted   0:21:40.00 – 0:21:42.00",
            "│(the line not shifted: its times point both ways)",
        ] {
            assert!(screen.contains(line), "{line}\n{screen}");
        }
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
    fn the_detail_pane_names_an_unnamed_track_by_its_language_bcp_47_first() {
        let mut m = sample();
        for file in &mut m.run.files {
            let track = &mut file.tracks[0];
            (track.name, track.language_bcp47) = (None, Some("ja".into()));
        }
        let screen = render(&m, 160, 20);
        assert!(
            screen.contains("0:02:45.06  (ja) [track 5 · sid 1]"),
            "{screen}"
        );
    }

    #[test]
    fn a_changed_file_and_a_status_show_on_the_last_line() {
        let mut m = sample();
        m.notes
            .insert(m.run.findings[0].file.clone(), FileNote::Changed);
        m.status = Some("not saved: disk full".into());
        let screen = render(&m, 160, 20);
        assert!(screen.contains("⚠ file changed since scan"), "{screen}");
        let lines: Vec<&str> = screen.lines().collect();
        assert!(lines[19].starts_with("not saved: disk full"), "{screen}");
        assert!(lines[19].ends_with(" ? help"), "{screen}");
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
        // 8 lines leave the detail pane 5 rows of 28 columns, so its lines wrap.
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
    fn the_status_line_ends_with_the_help_hint() {
        let mut m = sample();
        m.status = Some("⏸ 0:02:51.06".into());
        let screen = render(&m, 110, 20);
        let line = screen.lines().nth(19).unwrap();
        assert!(line.starts_with("⏸ 0:02:51.06 "), "{screen}");
        assert!(line.ends_with(" ? help"), "{screen}");
        // In the terminal's own colour, as the message is.
        let mut term = Terminal::new(TestBackend::new(110, 20)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        assert_eq!(term.backend().buffer()[(104, 19)].fg, Color::Reset);
        // A message that fits stops a column short of it.
        m.status = Some("x".repeat(103));
        let screen = render(&m, 110, 20);
        assert_eq!(
            screen.lines().nth(19).unwrap(),
            format!("{} ? help", "x".repeat(103))
        );
        // One too long for that takes the whole line.
        m.status = Some(format!("{}y", "x".repeat(109)));
        let screen = render(&m, 110, 20);
        assert_eq!(
            screen.lines().nth(19).unwrap(),
            format!("{}y", "x".repeat(109))
        );
    }

    #[test]
    fn the_files_view_shows_each_files_stage_and_the_selected_ones_log() {
        use crate::review::model::tests_support::path;
        use crate::scan::Event;
        let mut m = sample();
        let queued = vec![path(3).into(), path(4).into()];
        update::update(&mut m, Msg::Scan(Event::Queued(queued)), now());
        update::update(&mut m, Msg::Scan(Event::Reading(path(4).into())), now());
        let done = Event::Finished {
            path: path(3).into(),
            error: None,
            findings: 1,
            lines: vec!["[1/2] K-On! - E03 - Episode.mkv: 400 lines, 1 findings".into()],
        };
        update::update(&mut m, Msg::Scan(done), now());
        // Wide enough for the findings' whole title, progress and all.
        let screen = render(&m, 200, 20);
        assert!(screen.contains("showing all · scanning 1/2 ⠋ "), "{screen}");
        update::update(&mut m, Msg::Key(KeyCode::Tab), now());
        m.file_selected = 2;
        let screen = render(&m, 160, 20);
        assert!(
            screen.contains(" findings  files · 4 files · scanning 1/2 ⠋ "),
            "{screen}"
        );
        assert!(screen.contains("E01  ✓ 400 lines · 1 finding"), "{screen}");
        assert!(screen.contains("E03  ✓ 1 finding"), "{screen}");
        assert!(screen.contains("E04  ⠋ reading"), "{screen}");
        assert!(
            screen.contains("│[1/2] K-On! - E03 - Episode.mkv: 400 lines, 1 findings"),
            "{screen}"
        );
        assert!(screen.contains("scan 1/2 · 1 finding"), "{screen}");
    }

    #[test]
    fn an_empty_list_while_scanning_says_findings_are_coming() {
        let mut m = Model::new(
            crate::store::Run {
                name: "r".into(),
                ..crate::store::Run::default()
            },
            std::collections::HashMap::new(),
        );
        let queued = vec![crate::review::model::tests_support::path(1).into()];
        update::update(&mut m, Msg::Scan(crate::scan::Event::Queued(queued)), now());
        let screen = render(&m, 120, 12);
        assert!(
            screen.contains("No findings yet · Tab shows the files"),
            "{screen}"
        );
    }

    #[test]
    fn question_mark_shows_every_key_in_a_box_over_the_panes() {
        let mut m = sample();
        m.help = true;
        let screen = render(&m, 120, 24);
        for text in [
            "┌ keys ",
            "decide",
            "a  accept",
            "x  clear: back to pending",
            "PgUp PgDn  page",
            "Tab        findings, files",
            "␣    pause",
            "[ ]  a frame back, forward",
            "└ q quits · any other key closes this ",
        ] {
            assert!(screen.contains(text), "{text}\n{screen}");
        }
        // The three groups side by side on 120 columns: one heading line holds them all.
        assert!(
            screen
                .lines()
                .any(|l| l.contains("decide") && l.contains("move") && l.contains("mpv")),
            "{screen}"
        );
        // On 70 the last group goes under the others.
        let screen = render(&m, 70, 24);
        let heading = |name: &str| screen.lines().position(|l| l.contains(name)).unwrap();
        assert_eq!(heading("decide"), heading("move"), "{screen}");
        assert!(heading("mpv") > heading("decide"), "{screen}");
        // Too short for every key, it still says how to leave.
        let screen = render(&m, 50, 20);
        assert!(
            screen.contains("q quits · any other key closes"),
            "{screen}"
        );
    }

    fn with_cost() -> Model {
        let mut m = sample();
        m.run.spent.add(Some(&crate::llm::Usage {
            input_tokens: 1_234_567,
            output_tokens: 31_234,
            cost_usd: Some(3.21),
            ..crate::llm::Usage::default()
        }));
        m
    }

    #[test]
    fn review_opens_with_what_the_llm_calls_cost_beside_the_help_button() {
        let mut m = with_cost();
        let last = |m: &Model, width| render(m, width, 20).lines().nth(19).unwrap().to_string();
        let full = "LLM: 1 call · 1.23M in / 31.2k out tokens · $3.21 at API prices";
        let line = last(&m, 80);
        assert!(
            line.starts_with(full) && line.ends_with(" ? help"),
            "{line}"
        );
        // Narrower, the tokens go, then the calls: the price stays, and so does `? help`.
        let line = last(&m, 60);
        assert!(
            line.starts_with("LLM: 1 call · $3.21 at API prices ") && line.ends_with(" ? help"),
            "{line}"
        );
        let line = last(&m, 35);
        assert!(
            line.starts_with("LLM: $3.21 at API prices ") && line.ends_with(" ? help"),
            "{line}"
        );
        // Too narrow for even that beside the button: the price, and the button gives way.
        assert_eq!(last(&m, 20), "LLM: $3.21 at API pr");
        // A message of its own takes the line.
        m.status = Some("warning: /m/locked: skipped: Permission denied".into());
        assert!(
            last(&m, 80).starts_with("warning: /m/locked"),
            "{}",
            last(&m, 80)
        );
        // Gone after the first key, whatever the width after.
        m.status = None;
        update::update(&mut m, Msg::Key(KeyCode::Char('j')), now());
        update::update(&mut m, Msg::Resize(80, 20), now());
        assert_eq!(last(&m, 80).trim_start(), "? help");
    }

    #[test]
    fn any_click_or_wheel_ends_the_cost_line_as_a_key_does() {
        use update::Mouse::{Click, WheelDown};
        // A row of the list, the wheel over the detail pane, an empty part of the status line,
        // and a click with the box of keys open.
        for (action, x, y, help) in [
            (Click, 5, 2, false),
            (WheelDown, 70, 5, false),
            (Click, 30, 19, false),
            (Click, 30, 5, true),
        ] {
            let mut m = with_cost();
            update::update(&mut m, Msg::Resize(80, 20), now());
            m.help = help;
            let status = layout::panes(m.area).status;
            assert!(m.status_text(status).is_some());
            update::update(&mut m, Msg::Mouse(action, x, y), now());
            assert_eq!(m.status_text(status), None, "{action:?} at {x},{y}");
        }
    }

    #[test]
    fn a_key_with_the_box_of_keys_open_ends_the_cost_line_too() {
        let mut m = with_cost();
        m.help = true;
        update::update(&mut m, Msg::Key(KeyCode::Esc), now());
        assert!(!m.help);
        assert!(!m.show_cost);
        // Ctrl-U, which comes as its own message.
        let mut m = with_cost();
        update::update(&mut m, Msg::KillLine, now());
        assert!(!m.show_cost);
    }

    #[test]
    fn a_click_lands_on_the_status_line_as_it_was_drawn() {
        // 20 columns: the cost line leaves no room for `? help`, so a click where it would be
        // presses nothing, and only ends the cost line.
        let mut m = with_cost();
        update::update(&mut m, Msg::Resize(20, 20), now());
        update::update(&mut m, Msg::Mouse(update::Mouse::Click, 17, 19), now());
        assert!(!m.help);
        assert!(!m.show_cost);
    }

    #[test]
    fn the_help_button_is_clickable_beside_the_cost_on_a_narrow_screen() {
        let mut m = with_cost();
        update::update(&mut m, Msg::Resize(60, 20), now());
        update::update(&mut m, Msg::Mouse(update::Mouse::Click, 57, 19), now());
        assert!(m.help);
    }

    #[test]
    fn the_keys_box_ends_with_what_the_runs_llm_calls_cost() {
        let mut m = sample();
        m.help = true;
        // None recorded: nothing is said of them.
        let screen = render(&m, 120, 30);
        assert!(!screen.contains("LLM"), "{screen}");
        m.run.spent.add(Some(&crate::llm::Usage {
            input_tokens: 1_234_567,
            output_tokens: 31_234,
            cost_usd: Some(3.21),
            ..crate::llm::Usage::default()
        }));
        let screen = render(&m, 120, 30);
        let lines: Vec<&str> = screen.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.contains("1 call · 1.23M in / 31.2k out tokens · $3.21 at API prices"))
            .unwrap_or_else(|| panic!("{screen}"));
        // Under the last row of keys: a blank line, a heading, then the costs.
        let last_key = lines
            .iter()
            .position(|l| l.contains("Tab        findings, files"));
        assert_eq!(last_key, Some(at - 3), "{screen}");
        assert!(lines[at - 1].contains("│ LLM "), "{screen}");
        // Too narrow for one line, it wraps between the parts.
        let screen = render(&m, 50, 30);
        assert!(
            screen.contains("│ 1 call · 1.23M in / 31.2k out tokens "),
            "{screen}"
        );
        assert!(screen.contains("│ $3.21 at API prices "), "{screen}");
    }

    #[test]
    fn the_list_stays_put_while_the_selection_moves_inside_it() {
        let mut m = crate::review::model::tests_support::many(12);
        // 10 lines: 1 for the footer and 2 for the border leave 7 rows.
        update::update(&mut m, Msg::Resize(110, 10), now());
        for _ in 0..8 {
            update::update(&mut m, Msg::Key(KeyCode::Char('j')), now());
        }
        for _ in 0..2 {
            update::update(&mut m, Msg::Key(KeyCode::Char('k')), now());
        }
        assert_eq!((m.selected, m.scroll), (6, 2));
        let screen = render(&m, 110, 10);
        let lines: Vec<&str> = screen.lines().collect();
        // Entries 2 to 8 show, E03 to E09, and the selection is on E07 inside them.
        assert!(lines[1].contains("E03 "), "{screen}");
        assert!(lines[5].contains("▶ · E07 "), "{screen}");
        assert!(lines[7].contains("E09 "), "{screen}");
    }

    #[test]
    fn a_taller_screen_shows_the_entries_above_instead_of_blank_rows() {
        let mut m = crate::review::model::tests_support::many(12);
        update::update(&mut m, Msg::Resize(110, 10), now());
        update::update(&mut m, Msg::Key(KeyCode::End), now());
        assert_eq!(m.scroll, 5);
        // 20 lines leave 17 rows: all 12 entries fit, from the top.
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
        m.filter = Filter::Only(State::Pending);
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

    /// The colour of the list line holding `text`, on a 160×20 screen.
    fn colour_of(m: &Model, text: &str) -> Color {
        let mut term = Terminal::new(TestBackend::new(160, 20)).unwrap();
        term.draw(|f| view(m, f)).unwrap();
        let screen = render(m, 160, 20);
        let y = screen.lines().position(|l| l.contains(text)).expect(text);
        let line = screen.lines().nth(y).unwrap();
        let x = line[..line.find(text).unwrap()].chars().count();
        term.backend().buffer()[(u16::try_from(x).unwrap(), u16::try_from(y).unwrap())].fg
    }

    #[test]
    fn decided_rows_stay_in_the_list_coloured_by_their_state() {
        let mut m = sample();
        let decide = |m: &mut Model, id, state| {
            m.run.decisions.insert(
                id,
                crate::store::Decision {
                    finding: id,
                    state,
                    edited: None,
                    at: jiff::Timestamp::UNIX_EPOCH,
                },
            );
        };
        assert_eq!(colour_of(&m, "everyday."), Color::Reset, "pending");
        decide(&mut m, 3, State::Rejected);
        assert_eq!(colour_of(&m, "everyday."), Color::Red);
        // Partly decided, partly pending: still to do, so not yellow.
        decide(&mut m, 1, State::Accepted);
        assert_eq!(colour_of(&m, "×3 0:02:45.06"), Color::Reset);
        for id in [1, 2, 4] {
            decide(&mut m, id, State::Accepted);
        }
        // The selected row too: its highlight reverses the colour.
        assert_eq!(colour_of(&m, "×3 0:02:45.06"), Color::Green);
        decide(&mut m, 2, State::Rejected);
        assert_eq!(colour_of(&m, "×3 0:02:45.06"), Color::Yellow, "mixed");
        // Each member in its own colour.
        m.expanded.insert(0);
        assert_eq!(colour_of(&m, "E01 0:02:45.06"), Color::Green);
        assert_eq!(colour_of(&m, "E02 0:02:45.06"), Color::Red);
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
        // The status line is the last, y 9; 30 columns leave no room for the editor's buttons.
        let status: String = (0..30)
            .map(|x| term.backend().buffer()[(x, 9)].symbol().to_string())
            .collect();
        // The 9-column prompt leaves 21: the text's last 20 characters, then the cursor.
        assert_eq!(status, "wording: 01234567890123456789 ");
        assert_eq!(term.get_cursor_position().unwrap(), Position::new(29, 9));
    }

    #[test]
    fn the_editors_buttons_show_on_a_60_column_screen() {
        let mut m = sample();
        m.editor = Some(super::super::model::Editor {
            entry: Entry::Row(0),
            text: "abc".into(),
            cursor: 3,
        });
        let screen = render(&m, 60, 20);
        let line = screen.lines().nth(19).unwrap();
        assert!(line.starts_with("wording: abc "), "{screen}");
        assert!(line.ends_with(" ⏎ accept · Esc cancel"), "{screen}");
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
    fn the_quit_question_shows_over_the_editor() {
        use crate::review::model::tests_support::path;
        let mut m = sample();
        let queued = vec![path(4).into(), path(5).into()];
        update::update(&mut m, Msg::Scan(crate::scan::Event::Queued(queued)), now());
        m.selected = 1;
        update::update(&mut m, Msg::Key(KeyCode::Char('e')), now());
        // Ctrl-C mid-edit: the next key answers the question, so it shows, as it does outside
        // the editor.
        update::update(&mut m, Msg::Quit, now());
        let screen = render(&m, 160, 20);
        let line = screen.lines().nth(19).unwrap();
        assert!(
            line.starts_with("scan is on 0/2 — stop it and quit? y/n "),
            "{screen}"
        );
        assert!(line.ends_with(" ? help"), "{screen}");
        // `n` lets it go, and the edit is as it was.
        update::update(&mut m, Msg::Key(KeyCode::Char('n')), now());
        let screen = render(&m, 160, 20);
        let line = screen.lines().nth(19).unwrap();
        assert!(
            line.starts_with("wording: Let's have snacks together every day. "),
            "{screen}"
        );
        assert!(line.ends_with(" ⏎ accept · Esc cancel"), "{screen}");
    }

    #[test]
    fn the_editor_takes_the_status_line_with_its_buttons() {
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
        assert_eq!((pos.x, pos.y), (10, 19));
        let screen = render(&m, 160, 20);
        let line = screen.lines().nth(19).unwrap();
        assert!(line.starts_with("wording: abc "), "{screen}");
        assert!(line.ends_with(" ⏎ accept · Esc cancel"), "{screen}");
    }
}
