//! The files view (`Tab`): one line per file of the run, with where the scan is with it; and in
//! the detail pane, what the scan said of the selected one.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use super::model::{Model, Pane};
use super::progress::{FileProgress, Stage};

pub fn files_pane(m: &Model, f: &mut Frame, area: Rect) {
    let p = &m.progress;
    let progress = p.title().map_or_else(String::new, |t| format!(" · {t}"));
    let mut title = super::view::tabs(Pane::Files);
    title.push(Span::raw(format!(" · {} files{progress} ", p.files.len())));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(title));
    if p.files.is_empty() {
        f.render_widget(Paragraph::new("No files yet.").block(block), area);
        return;
    }
    let spinner = p.spinner();
    let items: Vec<ListItem> = p
        .files
        .iter()
        .map(|file| ListItem::new(line(m, file, spinner)))
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_symbol("▶ ")
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default()
        .with_selected(Some(m.file_selected))
        .with_offset(m.files_scroll(f.area()));
    f.render_stateful_widget(list, area, &mut state);
}

/// A file's line: its name in the list and its stage, coloured: done green, failed red, queued
/// grey, under way the terminal's own.
fn line(m: &Model, file: &FileProgress, spinner: char) -> Line<'static> {
    let fg = |c| Style::default().fg(c);
    let style = match file.stage {
        Stage::Done { .. } => fg(Color::Green),
        Stage::Failed(_) => fg(Color::Red),
        Stage::Queued | Stage::QueuedAgain => fg(Color::DarkGray),
        Stage::Reading | Stage::Asking { .. } => Style::default(),
    };
    Line::styled(
        format!("{}  {}", m.label(&file.path), file.label(spinner)),
        style,
    )
}

/// The detail pane's lines for the selected file: its path, its stage, and its log.
pub fn detail_lines(m: &Model) -> Vec<Line<'static>> {
    let Some(file) = m.progress.files.get(m.file_selected) else {
        return Vec::new();
    };
    let mut lines = vec![
        Line::styled(file.path.clone(), Style::default().fg(Color::DarkGray)),
        Line::from(file.label(m.progress.spinner())),
        Line::default(),
    ];
    lines.extend(file.log.iter().map(|l| Line::from(l.clone())));
    lines
}
