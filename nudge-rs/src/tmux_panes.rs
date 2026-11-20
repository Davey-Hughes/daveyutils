//! Enumerate tmux panes, for the picker and for `-a`.

use anyhow::{Context, bail};

use crate::target::tmux::{SERVER_FORMAT, TITLE_FORMAT};

/// A parsed `list-panes` row: the human-facing [`Pane`](crate::panes::Pane)
/// fields plus the machine fields that choose the default selection and tell
/// the pane running herdr's client from the others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) server: Option<String>, // "1793226261-4820" — the same on every row
    pub(crate) pane_id: String,        // "%16"  — matches $TMUX_PANE
    pub(crate) window_id: String,      // "@15"  — groups panes per window
    pub(crate) is_last: bool,          // pane_last == "1"
    pub(crate) target: String,         // "main:4.0" — the existing Pane.target
    pub(crate) command: String,        // "herdr" — the pane's foreground program
    pub(crate) title: String,
}

/// The `-F` template: the machine fields come first so a title containing a
/// tab can never shift one; the title (last) keeps embedded tabs. The first is
/// [`SERVER_FORMAT`], the same on every row, read here so that it comes from
/// the same answer as the ids. The command, next to last, is what tells the
/// pane running herdr's client from the others.
fn format() -> String {
    format!(
        "{SERVER_FORMAT}\t#{{pane_id}}\t#{{window_id}}\t#{{pane_last}}\t#{{session_name}}:#{{window_index}}.#{{pane_index}}\t#{{pane_current_command}}\t{TITLE_FORMAT}"
    )
}

/// Parse the enriched `list-panes` output into [`Row`]s. Blank lines are
/// skipped; the last field (title) keeps any embedded tabs.
fn parse_rows(output: &str) -> Vec<Row> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            // splitn(7) keeps any tabs embedded in the title (the last field).
            let mut fields = l.splitn(7, '\t');
            let server = fields.next()?;
            let pane_id = fields.next()?;
            let window_id = fields.next()?;
            let is_last = fields.next()?;
            let target = fields.next()?;
            let command = fields.next()?;
            let title = fields.next().unwrap_or("");
            Some(Row {
                server: crate::target::tmux::parse_server(server),
                pane_id: pane_id.to_string(),
                window_id: window_id.to_string(),
                is_last: is_last == "1",
                target: target.to_string(),
                command: command.to_string(),
                title: title.to_string(),
            })
        })
        .collect()
}

/// Why the target pane could not be resolved.
///
/// Three causes rather than one, because they have three different remedies and
/// a user staring at "could not pick a pane" cannot tell which of them they are
/// in. Every message ends by naming `-p`, the one thing that always works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoTarget {
    /// `$TMUX_PANE` is unset — nudge is not running inside tmux.
    NotInTmux,
    /// `$TMUX_PANE` names a pane this server does not list.
    MyPaneNotListed(String),
    /// Nudge's window has no last-active pane other than nudge's own.
    NoLastActive,
}

impl std::fmt::Display for NoTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoTarget::NotInTmux => write!(
                f,
                "--auto targets the pane you were last in, but $TMUX_PANE is not \
                 set -- nudge is not running inside tmux. Pass -p <pane>."
            ),
            NoTarget::MyPaneNotListed(id) => write!(
                f,
                "--auto could not find nudge's own pane ({id}) among the tmux \
                 panes on this server. Pass -p <pane>."
            ),
            NoTarget::NoLastActive => write!(
                f,
                "--auto targets the pane you were in before this one, and this \
                 window has no other recently-used pane. Pass -p <pane>."
            ),
        }
    }
}

impl std::error::Error for NoTarget {}

/// Index of the target pane: the last-active pane of the window that holds `me`
/// (nudge's own pane, from `$TMUX_PANE`), or why that cannot be resolved.
///
/// The rule itself, stated once. `--auto`
/// ([`auto_locate`](crate::panes::auto_locate)) refuses when it has no answer;
/// the picker's preselection ([`list`](crate::panes::list)) asks `--auto` and
/// falls back to the first pane, for the reason its `default_idx` gives.
pub(crate) fn auto_idx(rows: &[Row], me: Option<&str>) -> Result<usize, NoTarget> {
    let me = me.ok_or(NoTarget::NotInTmux)?;
    let my = rows
        .iter()
        .find(|r| r.pane_id == me)
        .ok_or_else(|| NoTarget::MyPaneNotListed(me.to_string()))?;
    // The `pane_id != me` guard is belt-and-suspenders: tmux never flags the
    // active pane as last, but this makes "never target nudge's own pane" a
    // property of the function rather than a tmux invariant.
    rows.iter()
        .position(|r| r.window_id == my.window_id && r.is_last && r.pane_id != me)
        .ok_or(NoTarget::NoLastActive)
}

/// Run `tmux list-panes` and parse it.
///
/// Shared by [`list`](crate::panes::list), [`positions_for`](crate::panes::positions_for) and
/// [`auto_locate`](crate::panes::auto_locate) so the `-F` invocation and its
/// error handling exist once; two copies would be free to drift into disagreeing
/// about what a pane row even is.
pub(crate) fn list_rows() -> anyhow::Result<Vec<Row>> {
    let out = std::process::Command::new("tmux")
        .args(["list-panes", "-a", "-F", &format()])
        .output()
        .context("running tmux list-panes")?;
    if !out.status.success() {
        bail!(
            "tmux list-panes failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_rows(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single enriched `list-panes` line, for a pane running `claude`.
    fn line(pane_id: &str, window_id: &str, last: bool, target: &str, title: &str) -> String {
        format!(
            "1793226261-4820\t{pane_id}\t{window_id}\t{}\t{target}\tclaude\t{title}",
            if last { "1" } else { "0" }
        )
    }

    fn row(pane_id: &str, window_id: &str, is_last: bool, target: &str) -> Row {
        Row {
            server: None,
            pane_id: pane_id.into(),
            window_id: window_id.into(),
            is_last,
            target: target.into(),
            command: "sh".into(),
            title: String::new(),
        }
    }

    #[test]
    fn the_title_tmux_gives_a_pane_by_default_is_left_out_of_the_list() {
        let f = format();
        assert!(f.ends_with(TITLE_FORMAT), "{f}");
    }

    #[test]
    fn the_list_format_starts_with_the_server_stamp_and_has_seven_fields() {
        let f = format();
        assert!(f.starts_with(SERVER_FORMAT), "{f}");
        assert_eq!(f.split('\t').count(), 7, "{f}");
    }

    /// The command goes before the title, which has to stay last.
    #[test]
    fn the_list_format_asks_for_each_panes_command_before_its_title() {
        let f = format();
        assert!(
            f.ends_with(&format!("\t#{{pane_current_command}}\t{TITLE_FORMAT}")),
            "{f}"
        );
    }

    #[test]
    fn every_row_carries_the_server_it_was_read_from() {
        let rows = parse_rows(&line("%16", "@15", true, "main:4.0", "claude"));
        assert_eq!(rows[0].server.as_deref(), Some("1793226261-4820"));
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
        assert_eq!(r.command, "claude");
        assert_eq!(r.title, "claude");
    }

    /// The pane running herdr's client is told apart by its command.
    #[test]
    fn parse_rows_reads_each_panes_command() {
        let out = "1793226261-4820\t%3\t@1\t0\tmain:1.0\therdr\t~/p\twith\ttabs";
        let rows = parse_rows(out);
        assert_eq!(rows[0].command, "herdr");
        assert_eq!(rows[0].title, "~/p\twith\ttabs");
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

    // --- the `--auto` rule ----------------------------------------------------
    //
    // The picker preselects its first pane for every case these refuse
    // (`panes::list`). That is right for the dashboard, where the answer is a
    // visible preselection the user can change with the arrow keys before
    // pressing Enter, and wrong for `--auto`, where the first pane is whatever
    // sorts first across every session on the machine -- chosen unseen, and
    // injected into hours later.

    /// The three ways the target cannot be resolved, as (rows, me) pairs.
    fn unresolvable() -> Vec<(Vec<Row>, Option<&'static str>)> {
        vec![
            // Not in tmux at all: nothing tells us which pane nudge is in.
            (vec![row("%11", "@1", true, "s:0.1")], None),
            // $TMUX_PANE names a pane this server does not list.
            (vec![row("%11", "@1", true, "s:0.1")], Some("%99")),
            // Just me, in a window never split or switched.
            (vec![row("%10", "@1", false, "s:0.0")], Some("%10")),
        ]
    }

    #[test]
    fn auto_idx_resolves_the_last_active_pane_in_my_window() {
        let rows = [
            row("%10", "@1", false, "s:0.0"), // me
            row("%11", "@1", true, "s:0.1"),  // last-active in my window
            row("%12", "@1", false, "s:0.2"),
        ];
        assert_eq!(auto_idx(&rows, Some("%10")).unwrap(), 1);
    }

    #[test]
    fn auto_idx_reports_that_nudge_is_not_running_inside_tmux() {
        let rows = [row("%11", "@1", true, "s:0.1")];
        assert_eq!(auto_idx(&rows, None), Err(NoTarget::NotInTmux));
    }

    #[test]
    fn auto_idx_reports_when_nudges_own_pane_is_not_listed() {
        let rows = [row("%11", "@1", true, "s:0.1")];
        assert_eq!(
            auto_idx(&rows, Some("%99")),
            Err(NoTarget::MyPaneNotListed("%99".to_string()))
        );
    }

    #[test]
    fn auto_idx_reports_when_my_window_has_no_last_active_pane() {
        let rows = [row("%10", "@1", false, "s:0.0")];
        assert_eq!(auto_idx(&rows, Some("%10")), Err(NoTarget::NoLastActive));
    }

    /// A last-active pane in a *different* window is not a target: `--auto`
    /// means "the pane I was just in", and jumping sessions is not that.
    #[test]
    fn auto_idx_ignores_a_last_active_pane_in_another_window() {
        let rows = [
            row("%10", "@1", false, "s:0.0"), // me
            row("%20", "@2", true, "s:1.0"),  // last-active, different window
        ];
        assert_eq!(auto_idx(&rows, Some("%10")), Err(NoTarget::NoLastActive));
    }

    #[test]
    fn auto_idx_never_returns_my_own_pane_even_if_flagged_last() {
        let rows = [row("%10", "@1", true, "s:0.0")]; // me, spuriously is_last
        assert_eq!(auto_idx(&rows, Some("%10")), Err(NoTarget::NoLastActive));
    }

    /// A refusal is a dead end unless it names the way out, and `--auto` has
    /// exactly one: name the pane yourself.
    #[test]
    fn every_auto_idx_failure_names_the_way_out() {
        for (rows, me) in unresolvable() {
            let msg = auto_idx(&rows, me).unwrap_err().to_string();
            assert!(
                msg.contains("-p"),
                "every refusal must point at the flag that fixes it: {msg}"
            );
        }
    }
}
