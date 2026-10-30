//! Pure render of `Model` into a ratatui frame.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table, Tabs, Wrap};

use super::model::{FormField, MessageField, Model, Tab, VimMode, WhenMode};
use crate::app::human_countdown;

/// How many terminal rows `s` needs at `width`, greedily wrapping on spaces the
/// way `Paragraph`'s `Wrap` does. The footer sizes itself from this so a key
/// reference longer than the terminal wraps instead of losing its tail.
fn wrapped_height(s: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    let mut lines = 1u16;
    let mut col = 0usize;
    for word in s.split_whitespace() {
        let w = word.chars().count();
        if col == 0 {
            col = w;
        } else if col + 1 + w <= width {
            col += 1 + w;
        } else {
            lines = lines.saturating_add(1);
            col = w;
        }
        // A word wider than the terminal spills onto further lines of its own.
        while col > width {
            lines = lines.saturating_add(1);
            col -= width;
        }
    }
    lines
}

pub fn view(model: &Model, f: &mut Frame) {
    // The footer stays minimal — the vim mode (shown only here now, not in the
    // form title) plus the one affordance we advertise. `?` toggles the rest in.
    let footer = if model.show_help {
        full_keys(model).to_string()
    } else {
        model
            .status
            .0
            .clone()
            .unwrap_or_else(|| minimal_hint(model).to_string())
    };
    // Give the footer the rows it actually needs (capped, so a pathological
    // status can't eat the screen) — the key reference no longer fits on 80.
    let footer_h = wrapped_height(&footer, f.area().width).min(4);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(footer_h),
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

    f.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), chunks[2]);
}

/// The collapsed footer: the vim mode (form only) plus the one affordance we
/// advertise. Everything else hides behind `?` to cut the clutter.
fn minimal_hint(model: &Model) -> &'static str {
    match model.tab {
        Tab::Jobs => "[?] help",
        Tab::NewNudge => {
            let mode = model
                .form
                .picker
                .as_ref()
                .map_or(model.form.nav_mode, |p| p.mode);
            if mode == VimMode::Normal {
                "NORMAL · [?] help"
            } else {
                "INSERT · [?] help"
            }
        }
    }
}

/// The full keybind reference, shown only while help is toggled on (`?`).
fn full_keys(model: &Model) -> &'static str {
    match model.tab {
        Tab::Jobs => {
            "[↑↓] select  [c] cancel  [e] edit  [r] refresh  [Tab] new  [q] quit  ·  [?] hide"
        }
        Tab::NewNudge => match &model.form.picker {
            Some(p) if p.mode == VimMode::Normal => {
                "NORMAL · [j/k] move  [i] insert  [enter] pick  [esc/q] cancel"
            }
            Some(_) => "INSERT · type to filter  ·  [↑↓] move  [enter] pick  [esc] normal",
            None if model.form.nav_mode == VimMode::Normal => {
                "NORMAL · [j/k] field  [h/l] move/change  [i/a] insert  [x/dd] delete  [/] search  [enter] schedule+quit  [^S] schedule+jobs  [q] quit  [esc] jobs  ·  [?] hide"
            }
            // In Insert, `q` is a command only where it isn't a character you'd
            // be typing — so it is advertised only off a text field.
            None if model.form.focused_text().is_none() => {
                "INSERT · [↑↓] field  [←→] change  [space] toggle  [/] search  [enter] schedule+quit  [^S] schedule+jobs  [q] quit  [esc] normal"
            }
            None => {
                "INSERT · [↑↓] field  [←→] change  [space] toggle  [/] search  [enter] schedule+quit  [^S] schedule+jobs  [esc] normal  [^C] quit"
            }
        },
    }
}

fn jobs_view(model: &Model, f: &mut Frame, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Pending jobs");
    if model.jobs.is_empty() {
        f.render_widget(Paragraph::new("no pending nudge jobs").block(block), area);
        return;
    }
    // As wide as the longest label, so the id at the end of one is not cut.
    let (labels, widest) = crate::tmux_panes::job_labels(&model.jobs, &model.positions);
    let pane_width = u16::try_from(widest + 1).unwrap_or(u16::MAX).max(22);
    // The names get what the other columns leave, up to the longest one. They
    // get no column when no job has one, or when fewer cells are left than
    // the header needs. A name cut short still says which session it is; a
    // pane id cut short names another pane.
    let (names, widest_name) = crate::tmux_panes::job_names(&model.jobs, &model.positions);
    // Which columns show, and how wide: one list for the header, the widths
    // and every row. NAME's width is worked out from the others.
    let mut columns = [
        ("ID", 5, true),
        ("NAME", 0, false),
        ("PANE", pane_width, true),
        ("FIRES IN", 12, true),
        ("MSGS", 5, true),
        ("FLAGS", 6, true),
    ];
    // What the other columns take: each one and the space after it, and the
    // two borders less the last column's space.
    let others = columns
        .iter()
        .filter(|(.., on)| *on)
        .fold(1, |sum: u16, &(_, width, _)| {
            sum.saturating_add(width).saturating_add(1)
        });
    let name_width =
        cols(widest_name.max(4)).min(area.width.saturating_sub(others).saturating_sub(1));
    columns[1] = ("NAME", name_width, widest_name > 0 && name_width >= 4);
    let shown = |cells: [String; 6]| {
        cells
            .into_iter()
            .zip(columns)
            .filter_map(|(cell, (.., on))| on.then_some(cell))
    };
    let rows =
        model
            .jobs
            .iter()
            .zip(names)
            .zip(labels)
            .enumerate()
            .map(|(i, ((j, name), label))| {
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
                Row::new(shown([
                    j.id.to_string(),
                    name.to_string(),
                    label,
                    human_countdown(delta),
                    j.messages.len().to_string(),
                    flags,
                ]))
                .style(style)
            });
    let header = shown(columns.map(|(title, ..)| title.to_string()));
    let widths = columns
        .iter()
        .filter(|(.., on)| *on)
        .map(|&(_, width, _)| Constraint::Length(width));
    let table = Table::new(rows, widths)
        .header(Row::new(header))
        .block(block);
    f.render_widget(table, area);
}

/// The preview panel's content: the captured pane's ANSI parsed into styled
/// text, bottom-anchored to the last `inner_h` lines. Parsing (not raw
/// passthrough) confines a pane's escapes to SGR styling. Degrades to stripped
/// plain text on a parse error, and to a placeholder when there is no capture.
fn preview_text(preview: Option<&str>, inner_h: usize) -> Text<'static> {
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

// too_many_lines (126/100): this is one screen's layout, and it reads top to
// bottom in the order the widgets appear. Splitting it to satisfy the counter
// would mean helpers that each take `model`, `f`, and a slice of the same Rect
// tree -- more surface, same logic, and the row/cursor arithmetic below would
// no longer sit next to the rows it measures.
#[allow(clippy::too_many_lines)]
fn form_view(model: &Model, f: &mut Frame, area: Rect) {
    let form = &model.form;
    if form.picker.is_some() {
        return picker_view(model, f, area);
    }
    let pane = form
        .selected_pane()
        .map_or("(no panes)", |p| p.target.as_str());
    // The When field is a single editable control: Auto shows the detected time,
    // Manual shows the time you type (empty = Auto, so any text means Manual),
    // Keep (edit-only) leaves the job's time alone.
    let when = match form.when {
        WhenMode::Keep => "keep current fire time".to_string(),
        WhenMode::Auto => match &form.detected {
            Some(crate::detect::Detection::Reset(z)) => format!("auto → {z}"),
            Some(crate::detect::Detection::None) => "auto → no banner detected".to_string(),
            Some(crate::detect::Detection::Unreadable { gap, .. }) => {
                format!("auto → day unreadable ({gap:?})")
            }
            Some(crate::detect::Detection::Elapsed { time, .. }) => {
                format!("auto → reset already passed ({time})")
            }
            None => "auto → (select a pane)".to_string(),
        },
        WhenMode::Manual => format!("manual → {}", form.manual_time),
    };
    let message = match &form.message {
        MessageField::Editable(s) => s.clone(),
        MessageField::Preserved(n) => format!("{n} messages — edit via CLI"),
    };
    let mark = |field: FormField| if form.focus == field { "▶ " } else { "  " };
    let onoff = |b: bool| if b { "[x]" } else { "[ ]" };
    // Show how many retries auto-retry will actually schedule (from config, or a
    // carried job's remaining count) so the count isn't a mystery.
    let auto_retry_label = if form.auto_retry {
        let n = form.retry_base(model.defaults.retries);
        let unit = if n == 1 { "retry" } else { "retries" };
        format!("auto-retry ({n} {unit})")
    } else {
        "auto-retry".to_string()
    };
    // The focused text field and the char its cursor is on (Normal) or before
    // (Insert) — `None` on selector fields, which show no cursor.
    let editing = form.focused_text().map(|v| {
        let len = v.chars().count();
        let at = match form.nav_mode {
            VimMode::Normal => form.cursor.min(len.saturating_sub(1)),
            VimMode::Insert => form.cursor.min(len),
        };
        (form.focus, at)
    });

    // The message wraps under its label, onto as many rows as it needs and the
    // screen can spare: the preview keeps 3 rows, and the form 9 (borders, the
    // six other fields, a blank). Past that, the rows shown follow the cursor.
    let msg_label = format!("{}Message: ", mark(FormField::Message));
    let label_w = msg_label.chars().count();
    let starts = wrap_starts(
        &message,
        usize::from(area.width.saturating_sub(2)).saturating_sub(label_w),
    );
    let msg_rows = starts
        .len()
        .min(usize::from(area.height.saturating_sub(12)).max(1));
    let msg_cursor = match editing {
        Some((FormField::Message, at)) => Some(wrapped_position(&starts, at)),
        _ => None,
    };
    let top = msg_cursor.map_or(0, |(row, _)| (row + 1).saturating_sub(msg_rows));
    let msg_chars: Vec<char> = message.chars().collect();
    let msg_txt = (top..top + msg_rows).map(|row| {
        let end = starts.get(row + 1).copied().unwrap_or(msg_chars.len());
        let text: String = msg_chars[starts[row]..end].iter().collect();
        if row == top {
            format!("{msg_label}{text}")
        } else {
            format!("{}{text}", " ".repeat(label_w))
        }
    });

    // Field rows, in `FORM_ORDER`. Kept as strings so the cursor can be placed at
    // its exact column below.
    let when_txt = format!("{}When:    {}", mark(FormField::When), when);
    let rows_txt: Vec<String> = [format!("{}Pane:    {}", mark(FormField::Pane), pane)]
        .into_iter()
        .chain([when_txt.clone()])
        .chain(msg_txt)
        .chain([
            format!("{}{} verify", mark(FormField::Verify), onoff(form.verify)),
            format!("{}{} notify", mark(FormField::Notify), onoff(form.notify)),
            format!(
                "{}{} {}",
                mark(FormField::AutoRetry),
                onoff(form.auto_retry),
                auto_retry_label
            ),
            format!("{}[ Schedule ]", mark(FormField::Submit)),
        ])
        .collect();
    let mut lines: Vec<Line> = rows_txt.iter().map(|s| Line::from(s.clone())).collect();
    // The vim mode lives in the bottom bar now — no duplicate in this title.
    let title = match form.mode {
        super::model::Mode::New => "nudge",
        super::model::Mode::Editing(_) => "edit nudge",
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3), // preview grows
            Constraint::Length(cols(9 + msg_rows)),
        ])
        .split(area);
    let preview_area = rows[0];
    let form_area = rows[1];

    // Preview panel (top): the selected pane's screen, bottom-anchored so the
    // banner (at the pane's bottom) stays visible when the panel is shorter.
    let pane_name = form
        .selected_pane()
        .map_or("(no pane)", |p| p.target.as_str());
    let preview_block = Block::default()
        .borders(Borders::ALL)
        .title(format!("preview: {pane_name}"));
    let inner_h = preview_area.height.saturating_sub(2) as usize; // minus borders
    f.render_widget(
        Paragraph::new(preview_text(form.preview.as_deref(), inner_h)).block(preview_block),
        preview_area,
    );

    // The cursor's row in the form and its column: the message's from the wrap,
    // When's (while Manual) after its label.
    let cursor = editing.map(|(field, at)| match (field, msg_cursor) {
        (FormField::Message, Some((row, col))) => (2 + row - top, label_w + col),
        _ => (
            1,
            when_txt.chars().count() - form.manual_time.chars().count() + at,
        ),
    });

    // Normal mode draws a block cursor (reverse video on the char under it) by
    // rebuilding that row's spans. No cursor-style escape is emitted, so the
    // terminal's own cursor shape/blink is untouched (Insert uses it below).
    if form.nav_mode == VimMode::Normal
        && let Some((idx, col)) = cursor
    {
        let rchars: Vec<char> = rows_txt[idx].chars().collect();
        let before: String = rchars[..col.min(rchars.len())].iter().collect();
        let under = rchars.get(col).copied().unwrap_or(' ');
        let after: String = rchars
            .get(col + 1..)
            .map_or(String::new(), |c| c.iter().collect());
        lines[idx] = Line::from(vec![
            Span::raw(before),
            Span::styled(
                under.to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ),
            Span::raw(after),
        ]);
    }

    let form_block = Block::default().borders(Borders::ALL).title(title);
    let inner = form_block.inner(form_area);
    f.render_widget(Paragraph::new(lines).block(form_block), form_area);

    // Live-validate the manual time: colour it red when it's non-empty but won't
    // parse, so a bad time reads as wrong before you schedule it. Parsing is a
    // pure function of the text (relative specs resolve against `now`, but
    // whether it parses does not depend on `now`), so it's fine to do here.
    if form.when == WhenMode::Manual && !form.manual_time.is_empty() {
        let now_zoned = model.now.to_zoned(model.defaults.tz.clone());
        if crate::timespec::parse_timespec(&form.manual_time, &now_zoned).is_err() {
            let value_len = cols(form.manual_time.chars().count());
            let vstart = cols(when_txt.chars().count()).saturating_sub(value_len);
            let y = inner.y + 1; // the When row
            let buf = f.buffer_mut();
            for col in 0..value_len {
                let x = inner.x + vstart + col;
                if x < inner.right() && y < inner.bottom() {
                    buf[(x, y)].set_fg(Color::Red);
                }
            }
        }
    }

    // Insert mode places the terminal's own cursor (a bar) at the edit position —
    // position+show only, so it keeps the terminal's shape and blink setting.
    if form.nav_mode == VimMode::Insert
        && let Some((idx, col)) = cursor
    {
        let cursor_x = (inner.x + cols(col)).min(inner.right().saturating_sub(1));
        let cursor_y = (inner.y + cols(idx)).min(inner.bottom().saturating_sub(1));
        f.set_cursor_position((cursor_x, cursor_y));
    }
}

/// Where each row of `value` starts, in chars, wrapped to `width` columns. A
/// row breaks after its last space, the space staying at the end of the row,
/// or mid-word when a word fills the row by itself. A last row as wide as the
/// field breaks too, so the cursor after the last char has a column.
fn wrap_starts(value: &str, width: usize) -> Vec<usize> {
    let chars: Vec<char> = value.chars().collect();
    let width = width.max(1);
    let mut starts = vec![0];
    let mut start = 0;
    while chars.len() - start >= width {
        let row = &chars[start..start + width];
        start += row
            .iter()
            .rposition(|&c| c == ' ')
            .map_or(width, |space| space + 1);
        starts.push(start);
    }
    starts
}

/// The row of `starts` that char `at` falls on, and its column there.
fn wrapped_position(starts: &[usize], at: usize) -> (usize, usize) {
    let row = starts.partition_point(|&s| s <= at).saturating_sub(1);
    (row, at - starts[row])
}

/// A character count as a terminal column/row offset.
///
/// Ratatui measures the screen in `u16`, so a count past `u16::MAX` is already
/// far off the right-hand edge; saturating there lands it outside the visible
/// area, which is where the callers' own `.min(inner.right())` clamps put it
/// regardless. Truncating with `as` would instead wrap it back on-screen at a
/// nonsense column.
fn cols(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
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
    // The product is a fraction of a u16 height, so it always fits; try_from
    // keeps that a fact rather than an assumption if PICKER_HEIGHT_PCT changes.
    let picker_h = u16::try_from(u32::from(area.height) * PICKER_HEIGHT_PCT / 100)
        .unwrap_or(u16::MAX)
        .max(4);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(picker_h)])
        .split(area);
    let preview_area = rows[0];
    let picker_area = rows[1];

    // Live preview of the highlighted pane (top) — same source as the form.
    let name = form
        .active_pane()
        .map_or("(no pane)", |p| p.target.as_str());
    let preview_block = Block::default()
        .borders(Borders::ALL)
        .title(format!("preview: {name}"));
    let inner_h = preview_area.height.saturating_sub(2) as usize;
    f.render_widget(
        Paragraph::new(preview_text(form.preview.as_deref(), inner_h)).block(preview_block),
        preview_area,
    );

    // Search + list block (bottom). The search line is pinned to the BOTTOM of
    // the block, below the list, so the line you type into stays put while the
    // match list resizes above it (fzf-style) rather than jumping around.
    let block = Block::default()
        .borders(Borders::ALL)
        .title(match picker.mode {
            VimMode::Insert => "pick a pane · INSERT",
            VimMode::Normal => "pick a pane · NORMAL",
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
    if picker.mode == VimMode::Insert {
        let cursor_x = (search_area.x + 2 + cols(picker.query.chars().count()))
            .min(search_area.right().saturating_sub(1));
        f.set_cursor_position((cursor_x, search_area.y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::job::TargetSpec;

    use super::super::model::{Model, ScheduleDefaults, VimMode, WhenMode};

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
    fn the_footer_advertises_only_help_until_toggled() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::Jobs;
        let collapsed = render(&m);
        assert!(collapsed.contains("[?] help"), "{collapsed}");
        assert!(
            !collapsed.contains("[c] cancel"),
            "the keys stay hidden until asked for: {collapsed}"
        );

        m.show_help = true;
        let expanded = render(&m);
        assert!(expanded.contains("[c] cancel"), "{expanded}");
        assert!(expanded.contains("[e] edit"), "{expanded}");
    }

    #[test]
    fn wrapped_height_counts_the_rows_a_line_needs() {
        assert_eq!(wrapped_height("", 80), 1);
        assert_eq!(wrapped_height("abc def", 80), 1);
        assert_eq!(
            wrapped_height("abc def", 7),
            1,
            "an exact fit stays one row"
        );
        assert_eq!(wrapped_height("abc def", 6), 2);
        assert_eq!(
            wrapped_height("abcdefgh", 4),
            2,
            "a word wider than the terminal spills"
        );
    }

    #[test]
    fn the_key_reference_wraps_rather_than_losing_its_tail() {
        // The form's reference no longer fits on 80 columns; the footer grows to
        // fit it so the last binding is not silently cut off.
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge;
        m.show_help = true;
        let out = render(&m);
        assert!(out.contains("[q]"), "{out}");
        assert!(out.contains("normal"), "the tail survives the wrap: {out}");
    }

    #[test]
    fn the_form_footer_stays_minimal_until_help_is_toggled() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge; // opens in Insert
        let collapsed = render(&m);
        assert!(collapsed.contains("INSERT · [?] help"), "{collapsed}");
        assert!(
            !collapsed.contains("[space] toggle"),
            "form keys hidden until asked for: {collapsed}"
        );

        m.show_help = true;
        assert!(render(&m).contains("[space] toggle"), "{}", render(&m));
    }

    #[test]
    fn a_job_row_shows_pane_and_a_countdown() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::Jobs;
        let mut j = crate::job::Job {
            id: 12,
            target: TargetSpec::Tmux {
                pane: "bot:0.1".into(),
                server: None,
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

    /// A job names its pane by tmux's id, and the row says where that pane
    /// sits now: after a renumber, that is not where it sat when scheduled.
    #[test]
    fn a_job_row_shows_where_its_pane_sits_now() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::Jobs;
        m.jobs = vec![crate::job::Job {
            id: 12,
            target: TargetSpec::Tmux {
                pane: "%19".into(),
                server: None,
            },
            messages: vec!["please continue".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-16T14:14:00Z".parse().unwrap(),
            notify: false,
            verify: false,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
        }];
        m.positions.panes = [("%19".to_string(), "main:11.0".to_string())].into();
        let out = render(&m);
        assert!(out.contains("main:11.0 (%19)"), "{out}");
    }

    fn job_in(pane: &str) -> crate::job::Job {
        crate::job::Job {
            id: 12,
            target: TargetSpec::Tmux {
                pane: pane.into(),
                server: None,
            },
            messages: vec!["please continue".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-16T14:14:00Z".parse().unwrap(),
            notify: false,
            verify: false,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
        }
    }

    #[test]
    fn a_job_row_names_the_session_in_its_pane() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::Jobs;
        m.jobs = vec![job_in("%13")];
        m.positions.panes = [("%13".to_string(), "main:5.0".to_string())].into();
        m.positions.names = [("%13".to_string(), "ra-scan-3".to_string())].into();
        let out = render(&m);
        let row = out.lines().find(|l| l.contains("%13")).unwrap_or("");
        assert!(row.contains("ra-scan-3"), "{out}");
        assert!(out.contains("NAME"), "{out}");
    }

    /// A name longer than the room left is cut, and the pane id is not.
    #[test]
    fn a_long_session_name_leaves_the_pane_id_whole() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::Jobs;
        m.jobs = vec![job_in("%202")];
        m.positions.panes = [("%202".to_string(), "main:10.0".to_string())].into();
        m.positions.names = [("%202".to_string(), "Nudge banner ".repeat(5))].into();
        let out = render(&m);
        assert!(out.contains("main:10.0 (%202)"), "{out}");
        assert!(out.contains("2h 14m"), "{out}");
        assert!(out.contains("FLAGS"), "{out}");
    }

    /// With no names to show, the table is as it was before it had a NAME
    /// column, so the PANE column still fits a narrow terminal.
    #[test]
    fn no_names_leave_no_name_column() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::Jobs;
        m.jobs = vec![job_in("%202")];
        m.positions.panes = [("%202".to_string(), "main:10.0".to_string())].into();
        let mut term = Terminal::new(TestBackend::new(58, 10)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        let buf = term.backend().buffer();
        let out: String = (0..10)
            .map(|y| (0..58).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();
        assert!(out.contains("main:10.0 (%202)"), "{out}");
        assert!(!out.contains("NAME"), "{out}");
    }

    /// A NAME column too narrow for its header says nothing, so a terminal
    /// that leaves it fewer than 4 cells has none.
    #[test]
    fn no_room_for_names_leaves_no_name_column() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::Jobs;
        m.jobs = vec![job_in("%202")];
        m.positions.panes = [("%202".to_string(), "main:10.0".to_string())].into();
        m.positions.names = [("%202".to_string(), "ra-scan-3".to_string())].into();
        let mut term = Terminal::new(TestBackend::new(59, 10)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        let buf = term.backend().buffer();
        let out: String = (0..10)
            .map(|y| (0..59).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();
        assert!(out.contains("main:10.0 (%202)"), "{out}");
        assert!(out.contains("ID    PANE"), "{out}");
    }

    /// The PANE column fits its longest label, so the id at the end of one is
    /// not cut off.
    #[test]
    fn a_long_pane_label_is_shown_whole() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::Jobs;
        m.jobs = vec![crate::job::Job {
            id: 12,
            target: TargetSpec::Tmux {
                pane: "%142".into(),
                server: None,
            },
            messages: vec!["please continue".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-16T14:14:00Z".parse().unwrap(),
            notify: false,
            verify: false,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
        }];
        m.positions.panes = [("%142".to_string(), "subcheck-review:3.0".to_string())].into();
        let out = render(&m);
        assert!(out.contains("subcheck-review:3.0 (%142)"), "{out}");
    }

    /// One very long session name must not push the other columns off a
    /// normal terminal.
    #[test]
    fn a_very_long_pane_label_leaves_room_for_the_other_columns() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::Jobs;
        m.jobs = vec![crate::job::Job {
            id: 12,
            target: TargetSpec::Tmux {
                pane: "%1".into(),
                server: None,
            },
            messages: vec!["please continue".into()],
            send_delay_secs: 0.75,
            fire_at: "2026-07-16T14:14:00Z".parse().unwrap(),
            notify: false,
            verify: true,
            auto_retry: false,
            retries_left: 0,
            settle_secs: 5.0,
            verify_fingerprint: None,
            verify_dims: None,
        }];
        m.positions.panes = [("%1".to_string(), format!("{}:1.0", "x".repeat(60)))].into();
        let out = render(&m);
        assert!(out.contains("2h 14m"), "{out}");
        assert!(out.contains("FLAGS"), "{out}");
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
    fn the_vim_mode_shows_in_the_bottom_bar_not_the_form_title() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge; // opens in Insert
        let out = render(&m);
        assert!(
            out.contains("INSERT · [?] help"),
            "the mode is in the bottom bar: {out}"
        );
        assert!(
            !out.contains("nudge · INSERT"),
            "and not duplicated in the form title: {out}"
        );
        m.form.nav_mode = VimMode::Normal;
        assert!(render(&m).contains("NORMAL · [?] help"), "{}", render(&m));
    }

    #[test]
    fn the_form_shows_a_pane_preview_on_top() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.form.panes = vec![crate::tmux_panes::Pane {
            id: "%1".into(),
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
                id: format!("%{i}"),
                target: format!("s:0.{i}"),
                title: format!("pane{i}"),
            })
            .collect();
        m.form.picker = Some(super::super::model::Picker {
            query: String::new(),
            matches: (0..30).collect(),
            highlight: 27,
            mode: super::super::model::VimMode::Insert,
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
            id: "%1".into(),
            target: "bot:0.1".into(),
            title: "claude".into(),
        }];
        m.form.picker = Some(super::super::model::Picker {
            query: "cl".into(),
            matches: vec![0],
            highlight: 0,
            mode: super::super::model::VimMode::Insert,
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
        let text = preview_text(Some("\x1b[31mRED\x1b[0m"), 10);
        assert_eq!(text.lines.len(), 10, "padded to the panel height");
        let last = text.lines.last().unwrap();
        assert_eq!(last.spans[0].content.as_ref(), "RED");
        assert_eq!(last.spans[0].style.fg, Some(ratatui::style::Color::Red));
    }

    #[test]
    fn a_missing_capture_previews_unavailable() {
        let text = preview_text(None, 10);
        assert_eq!(
            text.lines[0].spans[0].content.as_ref(),
            "(preview unavailable)"
        );
    }

    #[test]
    fn the_picker_puts_the_cursor_after_the_query() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.form.panes = vec![crate::tmux_panes::Pane {
            id: "%1".into(),
            target: "bot:0.1".into(),
            title: "claude".into(),
        }];
        m.form.picker = Some(super::super::model::Picker {
            query: "cl".into(),
            matches: vec![0],
            highlight: 0,
            mode: super::super::model::VimMode::Insert,
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

    #[test]
    fn insert_mode_puts_the_cursor_after_the_focused_text_field() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge; // opens in Insert
        m.form.focus = FormField::Message;
        m.form.message = MessageField::Editable("hi".into());
        m.form.cursor = 2; // at the end
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        // Message is row 2 of the form block; the block sits below the 8-row
        // preview (body y=1..18, preview 8 rows, form starts y=9 → inner y=10).
        // x = block inset(1) + "▶ Message: "(11) + cursor 2 = 14.
        let pos = term.get_cursor_position().unwrap();
        assert_eq!(pos.x, 14, "just after '▶ Message: hi'");
        assert_eq!(pos.y, 12, "on the Message row (inner y 10 + row 2)");
    }

    #[test]
    fn the_manual_when_field_gets_the_cursor() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge; // Insert
        m.form.focus = FormField::When;
        m.form.when = WhenMode::Manual;
        m.form.manual_time = "3pm".into();
        m.form.cursor = 3; // at the end
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        // When is row 1; x = inset(1) + "▶ When:    manual → "(20) + cursor 3 = 24.
        let pos = term.get_cursor_position().unwrap();
        assert_eq!(pos.x, 24, "just after '▶ When:    manual → 3pm'");
        assert_eq!(pos.y, 11, "on the When row (inner y 10 + row 1)");
    }

    #[test]
    fn insert_on_a_blank_when_shows_an_empty_manual_input() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge; // Insert
        m.form.focus = FormField::When;
        m.form.when = WhenMode::Manual;
        m.form.manual_time = String::new(); // blank
        m.form.cursor = 0;
        let out = render(&m);
        assert!(
            out.contains("manual → "),
            "shows the manual input, not auto: {out}"
        );
        assert!(!out.contains("auto →"), "does not fall back to auto: {out}");
        // Cursor sits just after "manual → ": inset(1) + "▶ When:    manual → "(20) = 21.
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        assert_eq!(term.get_cursor_position().unwrap().x, 21);
    }

    #[test]
    fn auto_retry_shows_the_configured_retry_count() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge; // defaults() configures retries = 2
        m.form.auto_retry = true;
        assert!(
            render(&m).contains("auto-retry (2 retries)"),
            "{}",
            render(&m)
        );
        // No count when it's off (it won't retry).
        m.form.auto_retry = false;
        let out = render(&m);
        assert!(out.contains("auto-retry"), "{out}");
        assert!(!out.contains("retries)"), "no count when off: {out}");
    }

    #[test]
    fn an_invalid_manual_time_is_shown_in_red() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge;
        m.form.focus = FormField::When;
        m.form.when = WhenMode::Manual;
        m.form.manual_time = "zzz".into(); // not a parseable time
        m.form.cursor = 3;
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        // The value starts at inset(1) + "▶ When:    manual → "(20) = 21, row y=11.
        assert_eq!(
            term.backend().buffer()[(21, 11)].fg,
            Color::Red,
            "an unparseable time is red"
        );

        // A valid time is not red.
        m.form.manual_time = "3pm".into();
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        assert_ne!(
            term.backend().buffer()[(21, 11)].fg,
            Color::Red,
            "a valid time is not red"
        );
    }

    #[test]
    fn normal_mode_draws_a_block_cursor_on_the_char() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge;
        m.form.focus = FormField::Message;
        m.form.message = MessageField::Editable("hi".into());
        m.form.cursor = 0; // on the 'h'
        m.form.nav_mode = VimMode::Normal;
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        // Message value starts at x = inset(1) + "▶ Message: "(11) = 12, row y=12.
        let buf = term.backend().buffer();
        let cell = &buf[(12, 12)];
        assert_eq!(
            cell.symbol(),
            "h",
            "block sits on the char under the cursor"
        );
        assert!(
            cell.modifier.contains(Modifier::REVERSED),
            "Normal-mode cursor is a reverse-video block"
        );
    }

    /// 60 a's, a space, 20 b's. At 80 columns the value has 78 - 11 = 67 to
    /// fill, so it breaks after the space: the a's and the space on the
    /// Message row, the b's on the row under it.
    fn two_row_message() -> String {
        format!("{} {}", "a".repeat(60), "b".repeat(20))
    }

    #[test]
    fn wrap_starts_breaks_after_the_last_space_that_fits() {
        assert_eq!(wrap_starts(&two_row_message(), 67), vec![0, 61]);
        assert_eq!(wrap_starts("", 67), vec![0]);
        assert_eq!(wrap_starts("please continue", 67), vec![0]);
    }

    #[test]
    fn wrap_starts_cuts_a_word_too_long_for_a_row() {
        assert_eq!(wrap_starts(&"x".repeat(100), 67), vec![0, 67]);
    }

    /// A full last row leaves the cursor after it nowhere to go but the edge,
    /// so it gets an empty row of its own.
    #[test]
    fn wrap_starts_gives_the_end_of_a_full_row_a_row_of_its_own() {
        assert_eq!(wrap_starts(&"x".repeat(67), 67), vec![0, 67]);
    }

    #[test]
    fn a_long_message_wraps_under_its_label() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge;
        m.form.message = MessageField::Editable(two_row_message());
        let out = render(&m);
        let rows: Vec<&str> = out.lines().collect();
        let first = rows
            .iter()
            .position(|r| r.contains("Message: aaa"))
            .unwrap_or_else(|| panic!("no Message row:\n{out}"));
        let under: String = rows[first + 1].chars().collect();
        assert_eq!(
            under.trim_end_matches([' ', '│']),
            format!("│{}{}", " ".repeat(11), "b".repeat(20)),
            "the b's continue under the a's:\n{out}"
        );
        assert!(
            out.contains("[ Schedule ]"),
            "rows below still show:\n{out}"
        );
    }

    #[test]
    fn insert_mode_puts_the_cursor_on_the_wrapped_row() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge;
        m.form.focus = FormField::Message;
        let message = two_row_message();
        m.form.cursor = message.chars().count();
        m.form.message = MessageField::Editable(message);
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        // The form is one row taller, so its inner area starts at y 9, not 10.
        // The b's are inner row 3; the cursor is after the 20th.
        let pos = term.get_cursor_position().unwrap();
        assert_eq!((pos.x, pos.y), (1 + 11 + 20, 12));
    }

    #[test]
    fn normal_mode_draws_the_block_cursor_on_the_wrapped_row() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge;
        m.form.focus = FormField::Message;
        m.form.message = MessageField::Editable(two_row_message());
        m.form.cursor = 70; // the tenth b
        m.form.nav_mode = VimMode::Normal;
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        let cell = &term.backend().buffer()[(1 + 11 + 9, 12)];
        assert_eq!(cell.symbol(), "b");
        assert!(cell.modifier.contains(Modifier::REVERSED));
    }

    /// Ten rows of message do not fit under an 80x20 screen's 3-row preview.
    /// The rows shown follow the cursor, and the form keeps its other rows.
    #[test]
    fn a_message_taller_than_the_screen_keeps_the_cursor_row_in_view() {
        let mut m = Model::new(defaults(), "2026-07-16T12:00:00Z".parse().unwrap());
        m.tab = Tab::NewNudge;
        m.form.focus = FormField::Message;
        let message = format!("{}{}", "x".repeat(67 * 9), "y".repeat(5));
        m.form.cursor = message.chars().count();
        m.form.message = MessageField::Editable(message);
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| view(&m, f)).unwrap();
        let pos = term.get_cursor_position().unwrap();
        let buf = term.backend().buffer();
        let row = |y: u16| -> String { (0..80).map(|x| buf[(x, y)].symbol()).collect() };
        assert!(
            row(pos.y).contains("yyyyy"),
            "the cursor's row is on screen: {:?}",
            row(pos.y)
        );
        assert_eq!(pos.x, 1 + 11 + 5);
        let screen: String = (0..20).map(row).collect::<Vec<_>>().join("\n");
        assert!(screen.contains("[ Schedule ]"), "{screen}");
        assert!(screen.contains("preview"), "{screen}");
    }
}
