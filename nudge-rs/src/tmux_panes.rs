//! Enumerate tmux panes for the interactive picker.

use anyhow::{bail, Context};

/// One selectable tmux pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub target: String,
    pub title: String,
}

/// A parsed `list-panes` row: the human-facing [`Pane`] fields plus the machine
/// fields used only to choose the default selection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    pane_id: String,   // "%16"  — matches $TMUX_PANE
    window_id: String, // "@15"  — groups panes per window
    is_last: bool,     // pane_last == "1"
    target: String,    // "main:4.0" — the existing Pane.target
    title: String,
}

/// The `-F` template: the three machine fields come first so a title containing
/// a tab can never shift a machine field; the title (5th) keeps embedded tabs.
const FORMAT: &str = "#{pane_id}\t#{window_id}\t#{pane_last}\t#{session_name}:#{window_index}.#{pane_index}\t#{pane_title}";

/// Parse the enriched `list-panes` output into [`Row`]s. Blank lines are
/// skipped; the 5th field (title) keeps any embedded tabs.
fn parse_rows(output: &str) -> Vec<Row> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            // splitn(5) keeps any tabs embedded in the title (the 5th field).
            let mut fields = l.splitn(5, '\t');
            let pane_id = fields.next()?;
            let window_id = fields.next()?;
            let is_last = fields.next()?;
            let target = fields.next()?;
            let title = fields.next().unwrap_or("");
            Some(Row {
                pane_id: pane_id.to_string(),
                window_id: window_id.to_string(),
                is_last: is_last == "1",
                target: target.to_string(),
                title: title.to_string(),
            })
        })
        .collect()
}

/// Index of the pane to pre-select: the last-active pane of the window that
/// holds `me` (nudge's own pane, from `$TMUX_PANE`). Falls back to `0` whenever
/// that cannot be resolved — not in tmux, `me` not in the list, or the window
/// has no last-active pane (a fresh single-pane window).
fn default_idx(rows: &[Row], me: Option<&str>) -> usize {
    let Some(me) = me else { return 0 };
    let Some(my) = rows.iter().find(|r| r.pane_id == me) else {
        return 0;
    };
    // The `pane_id != me` guard is belt-and-suspenders: tmux never flags the
    // active pane as last, but this makes "never default to nudge's own pane" a
    // property of the function rather than a tmux invariant.
    rows.iter()
        .position(|r| r.window_id == my.window_id && r.is_last && r.pane_id != me)
        .unwrap_or(0)
}

/// List all tmux panes across sessions, plus the index to pre-select (the
/// last-active pane of nudge's own window; see [`default_idx`]).
pub fn list() -> anyhow::Result<(Vec<Pane>, usize)> {
    let out = std::process::Command::new("tmux")
        .args(["list-panes", "-a", "-F", FORMAT])
        .output()
        .context("running tmux list-panes")?;
    if !out.status.success() {
        bail!(
            "tmux list-panes failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let rows = parse_rows(&String::from_utf8_lossy(&out.stdout));
    // Impure edge: read which pane nudge itself runs in.
    let me = std::env::var("TMUX_PANE").ok();
    let idx = default_idx(&rows, me.as_deref());
    let panes = rows
        .into_iter()
        .map(|r| Pane {
            target: r.target,
            title: r.title,
        })
        .collect();
    Ok((panes, idx))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single enriched `list-panes` line.
    fn line(pane_id: &str, window_id: &str, last: bool, target: &str, title: &str) -> String {
        format!(
            "{pane_id}\t{window_id}\t{}\t{target}\t{title}",
            if last { "1" } else { "0" }
        )
    }

    fn row(pane_id: &str, window_id: &str, is_last: bool, target: &str) -> Row {
        Row {
            pane_id: pane_id.into(),
            window_id: window_id.into(),
            is_last,
            target: target.into(),
            title: String::new(),
        }
    }

    #[test]
    fn parse_rows_lands_every_field() {
        let rows = parse_rows(&line("%16", "@15", true, "main:4.0", "claude"));
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.pane_id, "%16");
        assert_eq!(r.window_id, "@15");
        assert!(r.is_last);
        assert_eq!(r.target, "main:4.0");
        assert_eq!(r.title, "claude");
    }

    #[test]
    fn parse_rows_ignores_blank_lines() {
        let out = format!("\n\n{}\n\n", line("%1", "@1", false, "s:0.0", "x"));
        assert_eq!(parse_rows(&out).len(), 1);
    }

    #[test]
    fn parse_rows_tolerates_an_empty_title() {
        let rows = parse_rows(&line("%1", "@1", false, "s:0.0", ""));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "");
    }

    #[test]
    fn parse_rows_preserves_a_title_with_spaces_and_tabs() {
        let rows = parse_rows(&line("%1", "@1", false, "s:0.0", "a title\twith\ttabs"));
        assert_eq!(rows[0].title, "a title\twith\ttabs");
    }

    #[test]
    fn default_idx_picks_the_last_active_pane_in_my_window() {
        let rows = [
            row("%10", "@1", false, "s:0.0"), // me
            row("%11", "@1", true, "s:0.1"),  // last-active in my window
            row("%12", "@1", false, "s:0.2"),
        ];
        assert_eq!(default_idx(&rows, Some("%10")), 1);
    }

    #[test]
    fn default_idx_ignores_a_last_active_pane_in_another_window() {
        let rows = [
            row("%10", "@1", false, "s:0.0"), // me — my window has no last-active pane
            row("%20", "@2", true, "s:1.0"),  // last-active, but a different window
        ];
        assert_eq!(default_idx(&rows, Some("%10")), 0);
    }

    #[test]
    fn default_idx_falls_back_to_zero_when_not_in_tmux() {
        let rows = [row("%11", "@1", true, "s:0.1")];
        assert_eq!(default_idx(&rows, None), 0);
    }

    #[test]
    fn default_idx_falls_back_to_zero_when_my_pane_is_not_listed() {
        let rows = [row("%11", "@1", true, "s:0.1")];
        assert_eq!(default_idx(&rows, Some("%99")), 0);
    }

    #[test]
    fn default_idx_falls_back_to_zero_with_no_last_active_pane() {
        let rows = [row("%10", "@1", false, "s:0.0")]; // just me, freshly split
        assert_eq!(default_idx(&rows, Some("%10")), 0);
    }

    #[test]
    fn default_idx_never_returns_my_own_pane_even_if_flagged_last() {
        // tmux never flags the active pane as last, but the guard makes that a
        // property of the function, not a tmux invariant.
        let rows = [row("%10", "@1", true, "s:0.0")]; // me, spuriously is_last
        assert_eq!(default_idx(&rows, Some("%10")), 0);
    }
}
