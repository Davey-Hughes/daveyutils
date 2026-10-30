//! Enumerate tmux panes for the interactive picker.

use anyhow::{Context, bail};

use crate::target::tmux::{SERVER_FORMAT, TITLE_FORMAT};

/// One selectable tmux pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// tmux's id for the pane (`%16`): what a job stores, since `target` is a
    /// position that renumbering hands to another pane.
    pub id: String,
    pub target: String,
    pub title: String,
}

/// Where each pane sits now, keyed by tmux's id for it (`%19` -> `main:12.0`),
/// the name of the session in each pane that has one, and the server those
/// ids were read from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Positions {
    /// As [`SERVER_FORMAT`](crate::target::tmux::SERVER_FORMAT) reads it, or
    /// `None` if tmux could not be asked.
    pub server: Option<String>,
    pub panes: std::collections::HashMap<String, String>,
    /// [`session_name`]s, keyed the same way.
    pub names: std::collections::HashMap<String, String>,
}

impl Positions {
    pub fn new() -> Self {
        Self::default()
    }
}

/// [`Positions`] for every pane on the server, or none if tmux cannot list
/// them. It only ever labels a pane for a person, which is no reason to fail
/// whatever is being labelled.
pub fn positions() -> Positions {
    list_rows()
        .map(|rows| Positions {
            server: rows.first().and_then(|r| r.server.clone()),
            names: rows
                .iter()
                .filter_map(|r| Some((r.pane_id.clone(), session_name(&r.title)?)))
                .collect(),
            panes: rows.into_iter().map(|r| (r.pane_id, r.target)).collect(),
        })
        .unwrap_or_default()
}

/// How to show a job's pane to a person: where it sits now, then the id the
/// job stores, as `main:12.0 (%19)`. A pane tmux does not list (it closed, or
/// the job predates ids and stores a position) shows as stored. An id from an
/// earlier tmux server says so, since the pane at that id now is another one.
pub fn label(target: &crate::job::TargetSpec, positions: &Positions) -> String {
    let crate::job::TargetSpec::Tmux { pane, .. } = target;
    if target.from_earlier_server(positions.server.as_deref()) {
        return format!("{pane} (earlier tmux server)");
    }
    match positions.panes.get(pane) {
        Some(position) => format!("{position} ({pane})"),
        None => pane.clone(),
    }
}

/// The name of the session in a job's pane, if its title gives one. Not for
/// an id from an earlier tmux server: the pane at that id now is another one.
pub fn name<'a>(target: &crate::job::TargetSpec, positions: &'a Positions) -> Option<&'a str> {
    let crate::job::TargetSpec::Tmux { pane, .. } = target;
    if target.from_earlier_server(positions.server.as_deref()) {
        return None;
    }
    positions.names.get(pane).map(String::as_str)
}

/// A job's pane in a line of text: the [`name`] of its session, where it has
/// one, then its [`label`], as `ra-scan-3 · main:5.0 (%13)`.
pub fn named_label(target: &crate::job::TargetSpec, positions: &Positions) -> String {
    let label = label(target, positions);
    match name(target, positions) {
        Some(name) => format!("{name} · {label}"),
        None => label,
    }
}

/// The name of the session in a pane, from the pane's title, or `None` if it
/// gives none.
///
/// Claude Code titles its pane with a status glyph and the session's name:
/// `✳ warmer-sessions`, or the title it made up before a `/rename`. Codex
/// joins the items its `terminal_title` setting lists with ` | `, and a thread
/// id (`01a18bcf-23dc-…`) is no name. Any other title is taken as it is.
pub fn session_name(title: &str) -> Option<String> {
    let title = title.trim_start();
    let mut chars = title.chars();
    let title = match (chars.next(), chars.next()) {
        // Claude's glyphs (✳, and a spinner's) are all past ASCII, where a
        // shell's `~` or `#` is not.
        (Some(glyph), Some(' ')) if !glyph.is_ascii() && !glyph.is_alphanumeric() => chars.as_str(),
        _ => title,
    };
    let parts: Vec<&str> = title
        .split(" | ")
        .map(str::trim)
        .filter(|part| !part.is_empty() && !is_thread_id(part))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" | "))
}

/// Whether `s` is a UUID, or the start of one Codex cut short with `...`.
/// How much of the id is left before the cut depends on how long the title
/// is, so a cut one need only start like a UUID.
fn is_thread_id(s: &str) -> bool {
    const UUID: [usize; 5] = [8, 4, 4, 4, 12];
    let (s, cut) = match s.strip_suffix("...").or_else(|| s.strip_suffix('…')) {
        Some(s) => (s, true),
        None => (s, false),
    };
    let mut groups = s.split('-').peekable();
    let mut n = 0;
    while let Some(group) = groups.next() {
        let Some(&len) = UUID.get(n) else {
            return false;
        };
        // Only the last group before the cut may be short, and not the first.
        let fits = if cut && n > 0 && groups.peek().is_none() {
            group.len() <= len
        } else {
            group.len() == len
        };
        if !fits || !group.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
        n += 1;
    }
    cut || n == UUID.len()
}

/// Each job's [`label`], in order, and the widest one's width in chars: what
/// a PANE column needs to show every id whole.
pub fn job_labels(jobs: &[crate::job::Job], positions: &Positions) -> (Vec<String>, usize) {
    let labels: Vec<String> = jobs.iter().map(|j| label(&j.target, positions)).collect();
    let widest = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    (labels, widest)
}

/// Each job's [`name`] (empty where it has none), in order, and the widest
/// one's width in chars.
pub fn job_names<'a>(jobs: &[crate::job::Job], positions: &'a Positions) -> (Vec<&'a str>, usize) {
    let names: Vec<&str> = jobs
        .iter()
        .map(|j| name(&j.target, positions).unwrap_or(""))
        .collect();
    let widest = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
    (names, widest)
}

/// A parsed `list-panes` row: the human-facing [`Pane`] fields plus the machine
/// fields used only to choose the default selection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    server: Option<String>, // "1793226261-4820" — the same on every row
    pane_id: String,        // "%16"  — matches $TMUX_PANE
    window_id: String,      // "@15"  — groups panes per window
    is_last: bool,          // pane_last == "1"
    target: String,         // "main:4.0" — the existing Pane.target
    title: String,
}

/// The `-F` template: the machine fields come first so a title containing a
/// tab can never shift one; the title (last) keeps embedded tabs. The first is
/// [`SERVER_FORMAT`], the same on every row, read here so that it comes from
/// the same answer as the ids.
fn format() -> String {
    format!(
        "{SERVER_FORMAT}\t#{{pane_id}}\t#{{window_id}}\t#{{pane_last}}\t#{{session_name}}:#{{window_index}}.#{{pane_index}}\t{TITLE_FORMAT}"
    )
}

/// Parse the enriched `list-panes` output into [`Row`]s. Blank lines are
/// skipped; the last field (title) keeps any embedded tabs.
fn parse_rows(output: &str) -> Vec<Row> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            // splitn(6) keeps any tabs embedded in the title (the last field).
            let mut fields = l.splitn(6, '\t');
            let server = fields.next()?;
            let pane_id = fields.next()?;
            let window_id = fields.next()?;
            let is_last = fields.next()?;
            let target = fields.next()?;
            let title = fields.next().unwrap_or("");
            Some(Row {
                server: crate::target::tmux::parse_server(server),
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
/// Shared by [`list`] and [`auto_target`] so the `-F` invocation and its
/// error handling exist once; two copies would be free to drift into disagreeing
/// about what a pane row even is.
fn list_rows() -> anyhow::Result<Vec<Row>> {
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
/// last-active pane of nudge's own window; see [`default_idx`]) and the server
/// they were read from.
pub fn list() -> anyhow::Result<(Vec<Pane>, usize, Option<String>)> {
    let rows = list_rows()?;
    let server = rows.first().and_then(|r| r.server.clone());
    // Impure edge: read which pane nudge itself runs in.
    let me = std::env::var("TMUX_PANE").ok();
    let idx = default_idx(&rows, me.as_deref());
    let panes = rows
        .into_iter()
        .map(|r| Pane {
            id: r.pane_id,
            target: r.target,
            title: r.title,
        })
        .collect();
    Ok((panes, idx, server))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single enriched `list-panes` line.
    fn line(pane_id: &str, window_id: &str, last: bool, target: &str, title: &str) -> String {
        format!(
            "1793226261-4820\t{pane_id}\t{window_id}\t{}\t{target}\t{title}",
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
            title: String::new(),
        }
    }

    fn tmux(pane: &str, server: Option<&str>) -> crate::job::TargetSpec {
        crate::job::TargetSpec::Tmux {
            pane: pane.into(),
            server: server.map(Into::into),
        }
    }

    fn on(server: Option<&str>, panes: &[(&str, &str)]) -> Positions {
        Positions {
            server: server.map(Into::into),
            panes: panes
                .iter()
                .map(|(id, at)| ((*id).to_string(), (*at).to_string()))
                .collect(),
            names: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn a_pane_is_labelled_where_it_sits_now() {
        let p = on(Some("100-1"), &[("%19", "main:12.0")]);
        assert_eq!(label(&tmux("%19", Some("100-1")), &p), "main:12.0 (%19)");
        assert_eq!(label(&tmux("%19", None), &p), "main:12.0 (%19)");
    }

    /// After a restart `%19` is some new pane, and showing where that one
    /// sits would point at the wrong session.
    #[test]
    fn an_id_from_an_earlier_server_is_not_given_the_new_panes_position() {
        let p = on(Some("200-2"), &[("%19", "main:3.0")]);
        assert_eq!(
            label(&tmux("%19", Some("100-1")), &p),
            "%19 (earlier tmux server)"
        );
    }

    /// Not knowing the current server is no evidence either way.
    #[test]
    fn an_unknown_current_server_labels_as_before() {
        let p = on(None, &[("%19", "main:12.0")]);
        assert_eq!(label(&tmux("%19", Some("100-1")), &p), "main:12.0 (%19)");
    }

    /// Titles as Claude Code 2.1.312 and Codex 0.165.0 set them, from live
    /// panes on 2026-10-30.
    #[test]
    fn a_session_name_is_its_panes_title_less_the_parts_that_are_not_a_name() {
        for (title, name) in [
            // Claude: a status glyph, idle or working, then the session's name.
            ("✳ warmer-sessions", Some("warmer-sessions")),
            ("⠐ ra-scan-3", Some("ra-scan-3")),
            (
                "✳ Nudge banner detection and job naming",
                Some("Nudge banner detection and job naming"),
            ),
            // Codex with `terminal_title = ["thread", "thread-id"]`.
            ("ra-gui | 01a18bcf-23dc-72a1-9ee1-c8d89...", Some("ra-gui")),
            (
                "ra-gui | 01a18bcf-23dc-72a1-9ee1-c8d89a2b3c4d",
                Some("ra-gui"),
            ),
            ("01a18bcf-23dc-72a1-9ee1-c8d89...", None),
            // A longer thread name leaves less of the id before the cut.
            (
                "my-long-thread-name | 01a18bcf-23dc-72a1...",
                Some("my-long-thread-name"),
            ),
            ("ra-gui | 01a18bcf-23dc-…", Some("ra-gui")),
            ("deadbeef-cafe-babe", Some("deadbeef-cafe-babe")),
            ("notes | cafe...", Some("notes | cafe...")),
            // A shell's title is what it is.
            ("~/p/davehugheslegacy", Some("~/p/davehugheslegacy")),
            ("dead-end | notes", Some("dead-end | notes")),
            ("20261030-1", Some("20261030-1")),
            ("deadbeef-cafe", Some("deadbeef-cafe")),
            ("~ notes", Some("~ notes")),
            ("# build", Some("# build")),
            ("", None),
            ("  ", None),
            ("✳ ", None),
        ] {
            assert_eq!(session_name(title).as_deref(), name, "{title:?}");
        }
    }

    #[test]
    fn a_named_label_leads_with_the_session_name() {
        let mut p = on(Some("100-1"), &[("%13", "main:5.0"), ("%14", "main:6.0")]);
        p.names = [("%13".to_string(), "ra-scan-3".to_string())].into();
        assert_eq!(
            named_label(&tmux("%13", Some("100-1")), &p),
            "ra-scan-3 · main:5.0 (%13)"
        );
        assert_eq!(
            named_label(&tmux("%14", Some("100-1")), &p),
            "main:6.0 (%14)"
        );
    }

    /// The pane at the id now is another one, and so is its name.
    #[test]
    fn an_id_from_an_earlier_server_is_not_given_the_new_panes_name() {
        let mut p = on(Some("200-2"), &[("%13", "main:5.0")]);
        p.names = [("%13".to_string(), "ra-scan-3".to_string())].into();
        let target = tmux("%13", Some("100-1"));
        assert_eq!(name(&target, &p), None);
        assert_eq!(named_label(&target, &p), "%13 (earlier tmux server)");
    }

    #[test]
    fn the_title_tmux_gives_a_pane_by_default_is_left_out_of_the_list() {
        let f = format();
        assert!(f.ends_with(TITLE_FORMAT), "{f}");
    }

    #[test]
    fn the_list_format_starts_with_the_server_stamp_and_has_six_fields() {
        let f = format();
        assert!(f.starts_with(SERVER_FORMAT), "{f}");
        assert_eq!(f.split('\t').count(), 6, "{f}");
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
