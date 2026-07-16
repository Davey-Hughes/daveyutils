//! Pure render of `Model` into a ratatui frame.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table, Tabs};
use ratatui::Frame;

use super::model::{human_countdown, FormField, MessageField, Model, PickerMode, Tab, WhenMode};
use crate::job::TargetSpec;

pub fn view(model: &Model, f: &mut Frame) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    let titles = ["nudge", "Jobs"];
    let sel = match model.tab {
        Tab::NewNudge => 0,
        Tab::Jobs => 1,
    };
    f.render_widget(Tabs::new(titles.to_vec()).select(sel), chunks[0]);

    match model.tab {
        Tab::Jobs => jobs_view(model, f, chunks[1]),
        Tab::NewNudge => form_view(model, f, chunks[1]),
    }

    let hint = match model.tab {
        Tab::Jobs => "[↑↓] select  [c] cancel  [e] edit  [r] refresh  [Tab] new  [q] quit",
        Tab::NewNudge => match &model.form.picker {
            Some(p) if p.mode == PickerMode::Normal => {
                "NORMAL · [j/k] move  [i] insert  [enter] pick  [esc/q] cancel"
            }
            Some(_) => "INSERT · type to filter  ·  [↑↓] move  [enter] pick  [esc] normal",
            None => {
                "[↑↓] field  [←→] change  [space] toggle  [/] search  [enter] schedule  [Esc] back  [q] quit"
            }
        },
    };
    let status = model.status.0.clone().unwrap_or_else(|| hint.to_string());
    f.render_widget(Paragraph::new(status), chunks[2]);
}

fn jobs_view(model: &Model, f: &mut Frame, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Pending jobs");
    if model.jobs.is_empty() {
        f.render_widget(Paragraph::new("no pending nudge jobs").block(block), area);
        return;
    }
    let rows = model.jobs.iter().enumerate().map(|(i, j)| {
        let TargetSpec::Tmux { pane } = &j.target;
        let delta = j.fire_at.duration_since(model.now).as_secs();
        let flags: String = [(j.verify, 'v'), (j.notify, 'n'), (j.auto_retry, 'a')]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, c)| *c)
            .collect();
        let style = if i == model.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        Row::new(vec![
            j.id.to_string(),
            pane.clone(),
            human_countdown(delta),
            j.messages.len().to_string(),
            flags,
        ])
        .style(style)
    });
    let widths = [
        Constraint::Length(5),
        Constraint::Length(22),
        Constraint::Length(12),
        Constraint::Length(5),
        Constraint::Length(6),
    ];
    let table = Table::new(rows, widths)
        .header(Row::new(vec!["ID", "PANE", "FIRES IN", "MSGS", "FLAGS"]))
        .block(block);
    f.render_widget(table, area);
}

/// The preview panel's content: the captured pane's ANSI parsed into styled
/// text, bottom-anchored to the last `inner_h` lines. Parsing (not raw
/// passthrough) confines a pane's escapes to SGR styling. Degrades to stripped
/// plain text on a parse error, and to a placeholder when there is no capture.
fn preview_text(preview: &Option<String>, inner_h: usize) -> Text<'static> {
    use ansi_to_tui::IntoText;
    match preview {
        Some(screen) => {
            let text = screen
                .into_text()
                .unwrap_or_else(|_| Text::raw(strip_ansi_escapes::strip_str(screen)));
            let start = text.lines.len().saturating_sub(inner_h);
            let tail = &text.lines[start..];
            // Bottom-align: pad the top so the pane's tail (where the banner sits)
            // rests at the bottom of the panel.
            let pad = inner_h.saturating_sub(tail.len());
            let mut lines: Vec<Line> = std::iter::repeat_with(|| Line::from(""))
                .take(pad)
                .collect();
            lines.extend_from_slice(tail);
            Text::from(lines)
        }
        None => Text::from("(preview unavailable)"),
    }
}

fn form_view(model: &Model, f: &mut Frame, area: Rect) {
    let form = &model.form;
    if form.picker.is_some() {
        return picker_view(model, f, area);
    }
    let pane = form
        .selected_pane()
        .map(|p| p.target.as_str())
        .unwrap_or("(no panes)");
    let when = match form.when {
        WhenMode::Keep => "keep current time".to_string(),
        WhenMode::Auto => match &form.detected {
            Some(crate::detect::Detection::Reset(z)) => format!("auto → {}", z),
            Some(crate::detect::Detection::None) => "auto → no banner detected".to_string(),
            Some(crate::detect::Detection::Unreadable { gap, .. }) => {
                format!("auto → weekly, day unreadable ({gap:?})")
            }
            None => "auto → (select a pane)".to_string(),
        },
        WhenMode::Manual => format!("manual: {}", form.manual_time),
    };
    let message = match &form.message {
        MessageField::Editable(s) => s.clone(),
        MessageField::Preserved(n) => format!("{n} messages — edit via CLI"),
    };
    let mark = |field: FormField| if form.focus == field { "▶ " } else { "  " };
    let onoff = |b: bool| if b { "[x]" } else { "[ ]" };
    let lines = vec![
        Line::from(format!("{}Pane:    {}", mark(FormField::Pane), pane)),
        Line::from(format!("{}When:    {}", mark(FormField::When), when)),
        Line::from(format!(
            "{}Manual:  {}",
            mark(FormField::ManualTime),
            form.manual_time
        )),
        Line::from(format!("{}Message: {}", mark(FormField::Message), message)),
        Line::from(format!(
            "{}{} verify",
            mark(FormField::Verify),
            onoff(form.verify)
        )),
        Line::from(format!(
            "{}{} notify",
            mark(FormField::Notify),
            onoff(form.notify)
        )),
        Line::from(format!(
            "{}{} auto-retry",
            mark(FormField::AutoRetry),
            onoff(form.auto_retry)
        )),
        Line::from(Span::from(format!(
            "{}[ Schedule ]",
            mark(FormField::Submit)
        ))),
    ];
    let title = match form.mode {
        super::model::Mode::New => "nudge",
        super::model::Mode::Editing(_) => "edit nudge",
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(10)]) // preview grows, form fixed
        .split(area);
    let preview_area = rows[0];
    let form_area = rows[1];

    // Preview panel (top): the selected pane's screen, bottom-anchored so the
    // banner (at the pane's bottom) stays visible when the panel is shorter.
    let pane_name = form
        .selected_pane()
        .map(|p| p.target.as_str())
        .unwrap_or("(no pane)");
    let preview_block = Block::default()
        .borders(Borders::ALL)
        .title(format!("preview: {pane_name}"));
    let inner_h = preview_area.height.saturating_sub(2) as usize; // minus borders
    f.render_widget(
        Paragraph::new(preview_text(&form.preview, inner_h)).block(preview_block),
        preview_area,
    );

    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        form_area,
    );
}

/// The picker occupies this fraction of the tab body; the preview gets the rest.
/// Fixed (not content-sized) so it doesn't resize as the match count changes —
/// the list scrolls inside it instead.
const PICKER_HEIGHT_PCT: u32 = 50;

fn picker_view(model: &Model, f: &mut Frame, area: Rect) {
    let form = &model.form;
    let picker = form.picker.as_ref().unwrap();
    // Preview on top, the search + list block below at a fixed fraction of the
    // body. The block does NOT resize with the match count; its list scrolls.
    let picker_h = ((area.height as u32 * PICKER_HEIGHT_PCT / 100) as u16).max(4);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(picker_h)])
        .split(area);
    let preview_area = rows[0];
    let picker_area = rows[1];

    // Live preview of the highlighted pane (top) — same source as the form.
    let name = form
        .active_pane()
        .map(|p| p.target.as_str())
        .unwrap_or("(no pane)");
    let preview_block = Block::default()
        .borders(Borders::ALL)
        .title(format!("preview: {name}"));
    let inner_h = preview_area.height.saturating_sub(2) as usize;
    f.render_widget(
        Paragraph::new(preview_text(&form.preview, inner_h)).block(preview_block),
        preview_area,
    );

    // Search + list block (bottom). The search line is pinned to the BOTTOM of
    // the block, below the list, so the line you type into stays put while the
    // match list resizes above it (fzf-style) rather than jumping around.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(match picker.mode {
            PickerMode::Insert => "pick a pane · INSERT",
            PickerMode::Normal => "pick a pane · NORMAL",
        });
    let inner = block.inner(picker_area);
    f.render_widget(block, picker_area);
    let inner_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    let list_area = inner_rows[0];
    let search_area = inner_rows[1];

    // The match list, above the search line: a scrolling window that keeps the
    // highlight visible (when there are more matches than rows, the highlighted
    // row rides the bottom of the window), bottom-aligned so the results rest
    // just above the prompt.
    let h = list_area.height as usize;
    let m = picker.matches.len();
    let (start, end) = if m <= h {
        (0, m)
    } else {
        let end = (picker.highlight + 1).clamp(h, m);
        (end - h, end)
    };
    let mut lines = Vec::new();
    for row in start..end {
        let Some(p) = form.panes.get(picker.matches[row]) else {
            continue;
        };
        let mark = if row == picker.highlight {
            "▶ "
        } else {
            "  "
        };
        let style = if row == picker.highlight {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!("{mark}{:<20} {}", p.target, p.title),
            style,
        )));
    }
    if picker.matches.is_empty() {
        lines.push(Line::from("  (no matching panes)"));
    }
    // Bottom-align: pad the top so the results rest just above the search line.
    let pad = h.saturating_sub(lines.len());
    let mut padded: Vec<Line> = std::iter::repeat_with(|| Line::from(""))
        .take(pad)
        .collect();
    padded.extend(lines);
    f.render_widget(Paragraph::new(padded), list_area);

    // The search line, pinned at the bottom of the block.
    f.render_widget(Paragraph::new(format!("> {}", picker.query)), search_area);

    // In Insert mode, put the terminal cursor at the end of the query so the
    // search line reads as the text field it is. This only positions+shows the
    // cursor (no DECSCUSR style escape), so it keeps the terminal's own cursor
    // shape and blink setting. In Normal mode the highlighted row is the focus,
    // so no cursor is shown. x = "> " prefix + query width, clamped inside the
    // right edge; y = the pinned search line.
    if picker.mode == PickerMode::Insert {
        let cursor_x = (search_area.x + 2 + picker.query.chars().count() as u16)
            .min(search_area.right().saturating_sub(1));
        f.set_cursor_position((cursor_x, search_area.y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::super::model::{Model, ScheduleDefaults};

    fn render(model: &Model) -> String {
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| view(model, f)).unwrap();
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

    fn defaults() -> ScheduleDefaults {
        ScheduleDefaults {
            send_delay_secs: 0.75,
            settle_secs: 5.0,
            retries: 2,
            tz: jiff::tz::TimeZone::UTC,
        }
    }

    #[test]
    fn empty_jobs_tab_says_so() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::Jobs;
        assert!(render(&m).contains("no pending nudge jobs"));
    }

    #[test]
    fn a_job_row_shows_pane_and_a_countdown() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::Jobs;
        let mut j = crate::job::Job {
            id: 12,
            target: TargetSpec::Tmux {
                pane: "bot:0.1".into(),
            },
            messages: vec!["please continue".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-16T14:14:00Z".parse().unwrap(),
            notify: false,
            verify: true,
            auto_retry: true,
            retries_left: 2,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
        };
        j.messages = vec!["please continue".into()];
        m.jobs = vec![j];
        let out = render(&m);
        assert!(out.contains("bot:0.1"), "{out}");
        assert!(out.contains("2h 14m"), "{out}");
    }

    #[test]
    fn new_nudge_tab_shows_the_form_fields() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge;
        let out = render(&m);
        assert!(out.contains("Message"), "{out}");
        assert!(out.contains("please continue"), "{out}");
    }

    #[test]
    fn the_form_shows_a_pane_preview_on_top() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.form.panes = vec![crate::tmux_panes::Pane {
            target: "bot:0.1".into(),
            title: String::new(),
        }];
        m.form.preview = Some("current session limit · resets 3:00pm".into());
        let out = render(&m); // NewNudge is the default tab
        assert!(out.contains("preview: bot:0.1"), "{out}");
        assert!(out.contains("resets 3:00pm"), "{out}");
        assert!(out.contains("Message"), "form fields still render: {out}");
    }

    #[test]
    fn a_missing_preview_says_unavailable() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.form.preview = None;
        let out = render(&m);
        assert!(out.contains("preview unavailable"), "{out}");
    }

    #[test]
    fn the_picker_list_scrolls_to_keep_the_highlight_visible() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.form.panes = (0..30)
            .map(|i| crate::tmux_panes::Pane {
                target: format!("s:0.{i}"),
                title: format!("pane{i}"),
            })
            .collect();
        m.form.picker = Some(super::super::model::Picker {
            query: String::new(),
            matches: (0..30).collect(),
            highlight: 27,
            mode: super::super::model::PickerMode::Insert,
        });
        // 80x20: the fixed-height picker can't show 30 rows, so it scrolls.
        let out = render(&m);
        assert!(
            out.contains("pane27"),
            "the highlighted pane is scrolled into view: {out}"
        );
        assert!(
            !out.contains("pane0 "),
            "an early pane scrolled out of the window: {out}"
        );
    }

    #[test]
    fn the_tab_bar_lists_nudge_first_then_jobs() {
        let m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        let out = render(&m);
        let nudge = out.find("nudge").expect("nudge tab present");
        let jobs = out.find("Jobs").expect("Jobs tab present");
        assert!(nudge < jobs, "nudge is left of Jobs: {out}");
    }

    #[test]
    fn the_picker_renders_the_query_and_matches() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.form.panes = vec![crate::tmux_panes::Pane {
            target: "bot:0.1".into(),
            title: "claude".into(),
        }];
        m.form.picker = Some(super::super::model::Picker {
            query: "cl".into(),
            matches: vec![0],
            highlight: 0,
            mode: super::super::model::PickerMode::Insert,
        });
        let out = render(&m);
        assert!(out.contains("pick a pane"), "{out}");
        assert!(out.contains("> cl"), "{out}");
        assert!(out.contains("claude"), "{out}");
    }

    #[test]
    fn the_preview_renders_ansi_colors_as_styled_text() {
        // A red "RED" via SGR should parse into a span styled red. The preview is
        // bottom-aligned and padded to the panel height, so the content is on the
        // last line.
        let text = preview_text(&Some("\x1b[31mRED\x1b[0m".into()), 10);
        assert_eq!(text.lines.len(), 10, "padded to the panel height");
        let last = text.lines.last().unwrap();
        assert_eq!(last.spans[0].content.as_ref(), "RED");
        assert_eq!(last.spans[0].style.fg, Some(ratatui::style::Color::Red));
    }

    #[test]
    fn a_missing_capture_previews_unavailable() {
        let text = preview_text(&None, 10);
        assert_eq!(
            text.lines[0].spans[0].content.as_ref(),
            "(preview unavailable)"
        );
    }

    #[test]
    fn the_picker_puts_the_cursor_after_the_query() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.form.panes = vec![crate::tmux_panes::Pane {
            target: "bot:0.1".into(),
            title: "claude".into(),
        }];
        m.form.picker = Some(super::super::model::Picker {
            query: "cl".into(),
            matches: vec![0],
            highlight: 0,
            mode: super::super::model::PickerMode::Insert,
        });
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        // The search line is pinned to the bottom of the picker block; here it
        // lands on row 17 (just above the status line). x = "> "(2) + "cl"(2) + the
        // block's left inset(1) = 5. (Row tracks the layout; x is the contract.)
        let pos = term.get_cursor_position().unwrap();
        assert_eq!(pos.x, 5, "cursor sits just after '> cl'");
        assert_eq!(
            pos.y, 17,
            "on the picker's search line, pinned at the bottom"
        );
    }
}
