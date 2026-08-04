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

/// Why the target pane could not be resolved.
///
/// Three causes rather than one, because they have three different remedies and
/// a user staring at "could not pick a pane" cannot tell which of them they are
/// in. Every message ends by naming `-p`, the one thing that always works.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoTarget {
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
/// The rule itself, stated once. [`default_idx`] and [`auto_target`] are two
/// policies over this one answer — see [`default_idx`] for why they differ.
fn auto_idx(rows: &[Row], me: Option<&str>) -> Result<usize, NoTarget> {
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

/// Index of the pane to pre-select. Falls back to `0` whenever the target
/// cannot be resolved — not in tmux, `me` not in the list, or the window has no
/// last-active pane (a fresh single-pane window).
///
/// The clamp is right *here* and wrong for `--auto`: this answer is a visible
/// preselection in a form the user can change with the arrow keys before
/// pressing Enter, so a poor guess costs a keystroke. `--auto` never shows it to
/// anyone, so the same guess would inject into whatever pane happened to sort
/// first across every session on the machine. Hence [`auto_target`], which
/// refuses instead.
fn default_idx(rows: &[Row], me: Option<&str>) -> usize {
    auto_idx(rows, me).unwrap_or(0)
}

/// Run `tmux list-panes` and parse it.
///
/// Shared by [`list`] and [`auto_target`] so the `-F FORMAT` invocation and its
/// error handling exist once; two copies would be free to drift into disagreeing
/// about what a pane row even is.
fn list_rows() -> anyhow::Result<Vec<Row>> {
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
    Ok(parse_rows(&String::from_utf8_lossy(&out.stdout)))
}

/// The pane `--auto` targets, or an error explaining why there isn't one.
///
/// Same rule as the dashboard's preselection ([`auto_idx`]), opposite policy on
/// failure: nothing here is shown to the user before it is acted on, so an
/// unresolvable target is reported rather than guessed at.
pub fn auto_target() -> anyhow::Result<String> {
    let rows = list_rows()?;
    // Impure edge: read which pane nudge itself runs in.
    let me = std::env::var("TMUX_PANE").ok();
    let idx = auto_idx(&rows, me.as_deref())?;
    Ok(rows[idx].target.clone())
}

/// List all tmux panes across sessions, plus the index to pre-select (the
/// last-active pane of nudge's own window; see [`default_idx`]).
pub fn list() -> anyhow::Result<(Vec<Pane>, usize)> {
    let rows = list_rows()?;
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

    // --- the `--auto` policy over the same rule -------------------------------
    //
    // `default_idx` above answers 0 for every case these refuse. That is right
    // for the dashboard, where the answer is a visible preselection the user can
    // change with the arrow keys before pressing Enter, and wrong for `--auto`,
    // where 0 is whatever pane sorts first across every session on the machine --
    // chosen unseen, and injected into hours later.

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

    /// The two policies, pinned against each other: everything `--auto` refuses
    /// is still a 0 for the dashboard, which must keep opening either way.
    #[test]
    fn default_idx_clamps_every_auto_idx_failure_to_zero() {
        for (rows, me) in unresolvable() {
            assert!(auto_idx(&rows, me).is_err(), "precondition");
            assert_eq!(
                default_idx(&rows, me),
                0,
                "the dashboard preselects pane 0 rather than failing to open"
            );
        }
    }
}
