//! Where panes sit and what to call them, for tmux and herdr alike.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use crate::here::{Here, PaneArg};
use crate::job::TargetSpec;
use crate::target::herdr::{
    self, Foreground, ForegroundProcess, HerdrPane, HerdrSession, HerdrTarget,
};
use crate::target::tmux::{PaneLocation, TmuxTarget};
use crate::tmux_panes::{Row, auto_idx, list_rows};

/// One pane the picker offers, tmux's or herdr's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// Where the pane sits, as the list shows it: `main:4.0`, or a herdr
    /// pane's label again, which is already where it sits.
    pub target: String,
    pub title: String,
    /// What a job scheduled for the pane stores: tmux's id and server, which
    /// no renumbering moves, or herdr's id, socket and the conversation the
    /// pane held when it was listed.
    pub spec: TargetSpec,
}

/// Every pane the picker offers, tmux's then herdr's (see [`merge`]), and
/// which of them to preselect: the pane `-a` would find from where nudge is
/// running, or the first wherever `-a` would refuse (see [`default_idx`]).
///
/// `Err` only when neither tmux nor herdr could be asked: either one alone
/// missing (no tmux server, herdr not installed) leaves the other's panes, and
/// herdr with no session running simply has none to add. Every herdr call
/// goes through [`herdr::list_panes`] and [`HerdrTarget`], which name the
/// session they are for with `HERDR_SOCKET_PATH`.
pub fn list() -> Result<(Vec<Pane>, usize)> {
    let tmux = list_rows();
    let herdr = herdr::sessions().map(|sessions| {
        herdr_listing(sessions, herdr::list_panes, |socket, pane| {
            HerdrTarget::new(pane, socket).foreground()
        })
    });
    let herdr = match (&tmux, herdr) {
        (Err(t), Err(h)) => bail!("no tmux or herdr panes to pick from: {t:#}; {h:#}"),
        (_, herdr) => herdr.unwrap_or_default(),
    };
    let panes = merge(tmux, herdr);
    let found = auto_locate(&Here::from_env()).ok();
    let idx = default_idx(&panes, found.as_ref().map(|f| &f.spec));
    Ok((panes, idx))
}

/// Each running herdr session in `sessions`, with its panes and the ids of
/// those whose foreground runs tmux, asking herdr through `list_panes` (by
/// session socket) and `foreground` (by socket and pane id).
///
/// A session whose `pane list` fails is left out whole; the other sessions
/// are still offered. A pane whose `process-info` fails is left out alone,
/// since a pane closing between the two calls is normal and must not hide the
/// rest of its session. It cannot be shown to be free of tmux's client, so it
/// is not offered.
fn herdr_listing(
    sessions: Vec<HerdrSession>,
    list_panes: impl Fn(&Path) -> Result<Vec<HerdrPane>>,
    foreground: impl Fn(&Path, &str) -> Result<Foreground>,
) -> Vec<(HerdrSession, Vec<HerdrPane>, HashSet<String>)> {
    sessions
        .into_iter()
        .filter(|s| s.running)
        .filter_map(|session| {
            let mut runs_tmux = HashSet::new();
            let panes: Vec<HerdrPane> = list_panes(&session.socket)
                .ok()?
                .into_iter()
                .filter(|pane| {
                    let Ok(fg) = foreground(&session.socket, &pane.id) else {
                        return false;
                    };
                    // By `is_tmux`, not the name `tmux`: on Linux tmux renames
                    // its client `tmux: client`, and herdr reports that name.
                    if fg.processes.iter().any(ForegroundProcess::is_tmux) {
                        runs_tmux.insert(pane.id.clone());
                    }
                    true
                })
                .collect();
            Some((session, panes, runs_tmux))
        })
        .collect()
}

/// The panes the picker offers: tmux's rows, then each herdr session's panes
/// in the order given, less every pane that holds the other multiplexer's
/// client.
///
/// Such a pane passes what it is typed on to whichever pane has focus inside
/// it, so it is never the pane a nudge is for, and the pane inside is already
/// listed in its own multiplexer's half. A tmux pane says it runs herdr's
/// client by its command; a herdr pane whose foreground runs tmux is in its
/// session's set. Either half may be missing: a tmux that cannot be asked
/// adds nothing, and nor does a herdr with no session to list.
fn merge(
    tmux: Result<Vec<Row>>,
    herdr: Vec<(HerdrSession, Vec<HerdrPane>, HashSet<String>)>,
) -> Vec<Pane> {
    let tmux = tmux
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row.command != "herdr")
        .map(|row| Pane {
            spec: TargetSpec::Tmux {
                pane: row.pane_id,
                server: row.server,
            },
            target: row.target,
            title: row.title,
        });
    let herdr = herdr.into_iter().flat_map(|(session, panes, runs_tmux)| {
        panes
            .into_iter()
            .filter(move |pane| !runs_tmux.contains(&pane.id))
            .map(move |pane| {
                // The conversation the pane holds as listed, as `-p` would
                // stamp it, so a job from the picker gets the same check
                // before it fires.
                let spec = TargetSpec::Herdr {
                    pane: pane.id,
                    socket: session.socket.clone(),
                    agent_session: pane.agent_session,
                };
                let label = label(&spec, &Positions::new());
                Pane {
                    target: label,
                    title: pane.terminal_title.unwrap_or_default(),
                    spec,
                }
            })
    });
    tmux.chain(herdr).collect()
}

/// Index in `panes` of the pane to preselect: the one `-a` found (`found`),
/// matched whatever either was stamped with, else the first.
///
/// The fallback is right here and wrong for `-a`: this answer is a visible
/// preselection in a form the user can change with the arrow keys before
/// pressing Enter, so a poor guess costs a keystroke. `-a` never shows it to
/// anyone, so the same guess would inject into whatever pane happened to sort
/// first across every session on the machine. Hence [`auto_locate`], which
/// refuses instead.
fn default_idx(panes: &[Pane], found: Option<&TargetSpec>) -> usize {
    found
        .and_then(|found| panes.iter().position(|p| p.spec.same_pane(found)))
        .unwrap_or(0)
}

/// Where each tmux pane sits now, keyed by tmux's id for it (`%19` ->
/// `main:12.0`), the name of the session in each pane that has one, and the
/// server those ids were read from; and the name of the session in each herdr
/// pane that has one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Positions {
    /// As [`SERVER_FORMAT`](crate::target::tmux::SERVER_FORMAT) reads it, or
    /// `None` if tmux could not be asked.
    pub server: Option<String>,
    pub panes: std::collections::HashMap<String, String>,
    /// [`session_name`]s, keyed the same way.
    pub names: std::collections::HashMap<String, String>,
    /// [`session_name`]s of herdr panes, keyed by the socket of the herdr
    /// session that holds the pane and herdr's id for it. The socket is part
    /// of the key because every herdr session numbers its own panes.
    pub herdr_names: HashMap<(PathBuf, String), String>,
}

impl Positions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep where `spec`'s pane sits (`position`) and the session `name` in
    /// it. A tmux pane keeps both, by its id; a herdr pane's label is its id,
    /// so only its name is kept, by socket and id.
    pub fn insert(&mut self, spec: &TargetSpec, position: &str, name: Option<String>) {
        match spec {
            TargetSpec::Tmux { pane, .. } => {
                self.panes.insert(pane.clone(), position.to_string());
                if let Some(name) = name {
                    self.names.insert(pane.clone(), name);
                }
            }
            TargetSpec::Herdr { pane, socket, .. } => {
                if let Some(name) = name {
                    self.herdr_names
                        .insert((socket.clone(), pane.clone()), name);
                }
            }
        }
    }
}

/// [`Positions`] for labelling `targets` alone: every tmux pane on the
/// server, and every pane of every running herdr session only if one of
/// `targets` is a herdr pane. Nothing here fails: it only labels a pane for a
/// person, so a tmux or herdr that cannot be asked leaves its half empty.
///
/// For the daemon, before it fires the jobs that are due, and for the
/// dashboard and `--list`, which have a job list in hand. A herdr that has
/// stopped answering costs each command nudge sends it the whole of herdr's
/// timeout, and listing the panes of every session to label jobs that are all
/// tmux's would put that wait in front of every one of them.
pub fn positions_for<'a>(targets: impl IntoIterator<Item = &'a TargetSpec>) -> Positions {
    positions_with(targets, list_rows, herdr_names)
}

/// [`positions_for`], given how to list tmux's panes and herdr's names.
fn positions_with<'a>(
    targets: impl IntoIterator<Item = &'a TargetSpec>,
    rows: impl FnOnce() -> Result<Vec<Row>>,
    herdr_names: impl FnOnce() -> HashMap<(PathBuf, String), String>,
) -> Positions {
    let mut positions = tmux_positions(rows());
    if targets
        .into_iter()
        .any(|t| matches!(t, TargetSpec::Herdr { .. }))
    {
        positions.herdr_names = herdr_names();
    }
    positions
}

/// tmux's half of [`Positions`], from `list-panes`' rows, or none of it if
/// tmux could not be asked.
fn tmux_positions(rows: Result<Vec<Row>>) -> Positions {
    rows.map(|rows| Positions {
        server: rows.first().and_then(|r| r.server.clone()),
        names: rows
            .iter()
            .filter_map(|r| Some((r.pane_id.clone(), session_name(&r.title)?)))
            .collect(),
        panes: rows.into_iter().map(|r| (r.pane_id, r.target)).collect(),
        herdr_names: HashMap::new(),
    })
    .unwrap_or_default()
}

/// herdr's half of [`Positions`]: the [`session_name`] of each pane of every
/// running herdr session that has one.
fn herdr_names() -> HashMap<(PathBuf, String), String> {
    let mut names = HashMap::new();
    for session in crate::target::herdr::sessions()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.running)
    {
        // A session that stops answering between the two calls is skipped
        // rather than failing the others.
        let Ok(panes) = crate::target::herdr::list_panes(&session.socket) else {
            continue;
        };
        for pane in panes {
            if let Some(name) = pane.terminal_title.as_deref().and_then(session_name) {
                names.insert((session.socket.clone(), pane.id), name);
            }
        }
    }
    names
}

/// The name of the herdr session whose API socket is `socket`, from where
/// herdr keeps it (`.../sessions/<name>/herdr.sock`), or `None` for the
/// default session's, which is not under `sessions/`.
pub fn herdr_session_of(socket: &Path) -> Option<String> {
    let dir = socket.parent()?;
    if dir.parent()?.file_name()? != "sessions" {
        return None;
    }
    dir.file_name()?.to_str().map(String::from)
}

/// How to show a job's pane to a person.
///
/// A tmux pane shows where it sits now, then the id the job stores, as
/// `main:12.0 (%19)`. A pane tmux does not list (it closed, or the job
/// predates ids and stores a position) shows as stored. An id from an earlier
/// tmux server says so, since the pane at that id now is another one.
///
/// A herdr pane shows as `-p` takes it, with its session named whichever it
/// is: `herdr:beaver:w1:p2`, `herdr:default:w7:pX` for herdr's default
/// session. A label that left the default session out would mean, as `-p`, the
/// session nudge is running in, so it would not mean the same pane from inside
/// another session. herdr's id is where the pane sits, so there is nothing
/// more to show.
pub fn label(target: &TargetSpec, positions: &Positions) -> String {
    let (pane, socket) = match target {
        TargetSpec::Tmux { pane, .. } => (pane, None),
        TargetSpec::Herdr { pane, socket, .. } => (pane, Some(socket)),
    };
    if let Some(socket) = socket {
        let session = herdr_session_of(socket).unwrap_or_else(|| "default".into());
        return format!("herdr:{session}:{pane}");
    }
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
pub fn name<'a>(target: &TargetSpec, positions: &'a Positions) -> Option<&'a str> {
    let (pane, socket) = match target {
        TargetSpec::Tmux { pane, .. } => (pane, None),
        TargetSpec::Herdr { pane, socket, .. } => (pane, Some(socket)),
    };
    if let Some(socket) = socket {
        return positions
            .herdr_names
            .get(&(socket.clone(), pane.clone()))
            .map(String::as_str);
    }
    if target.from_earlier_server(positions.server.as_deref()) {
        return None;
    }
    positions.names.get(pane).map(String::as_str)
}

/// A job's pane in a line of text: the [`name`] of its session, where it has
/// one, then its [`label`], as `ra-scan-3 · main:5.0 (%13)`.
pub fn named_label(target: &TargetSpec, positions: &Positions) -> String {
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

/// A pane `-p` or `-a` found: what a job stores for it, where it sits now
/// (`main:12.0` for a tmux pane; for a herdr pane, its [`label`],
/// `herdr:w7:pX`), and the [`session_name`] in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub spec: TargetSpec,
    pub position: String,
    pub name: Option<String>,
}

/// The pane `arg` names. If it holds the other multiplexer's client, the pane
/// behind that client instead (see [`through_tmux`] and [`through_herdr`]).
pub fn locate(arg: &PaneArg, here: &Here) -> Result<Located> {
    match arg {
        PaneArg::Tmux(target) => {
            let loc = TmuxTarget::new(target.as_str()).locate()?;
            // By its id from here on: the target given may be a position,
            // which a window closing in between would hand to another pane.
            let command = TmuxTarget::new(loc.id.as_str()).current_command()?;
            let id = loc.id.clone();
            through_tmux(&id, tmux_located(loc), &command)
        }
        PaneArg::Herdr { session, pane } => {
            let socket = herdr_socket_for(session.as_deref(), here, herdr::sessions)?;
            herdr_at(pane, socket)
        }
    }
}

/// The pane `-a` means where nudge is running: the pane you were last in
/// beside nudge's tmux pane, or the pane under nudge's tmux or herdr popup.
/// If it holds the other multiplexer's client, the pane behind that client
/// instead. Anywhere else there is no rule to go by, and nothing is guessed.
pub fn auto_locate(here: &Here) -> Result<Located> {
    match here {
        // Both refusals come before anything is run: there is nothing to ask.
        Here::HerdrSplit { .. } => bail!(
            "--auto cannot tell from a herdr split which pane you were in before: \
             herdr does not say which pane had focus last. Run nudge -a from a herdr \
             popup over the agent's pane (a [[keys.command]] with type = \"popup\" in \
             herdr's config), or pass -p herdr:<workspace>:<pane>."
        ),
        Here::Outside => bail!(
            "--auto targets the pane you were last in, but nudge is running inside \
             neither tmux nor herdr. Pass -p <pane>: a tmux pane (main:1.0, %3) or a \
             herdr pane (herdr:w7:pX)."
        ),
        Here::TmuxPane { pane } => {
            let rows = list_rows()?;
            let row = &rows[auto_idx(&rows, Some(pane))?];
            through_tmux(&row.pane_id, row_located(row), &row.command)
        }
        // A popup is no pane, so it has no last pane either. tmux takes the
        // pane under it as the current one (seen 2026-11-19).
        Here::TmuxPopup => {
            let id = tmux_display(&["-p", "#{pane_id}"])?;
            locate(&PaneArg::Tmux(id), here)
        }
        // herdr leaves the pane under a popup focused, and names it to the
        // popup in `HERDR_ACTIVE_PANE_ID`.
        Here::HerdrPopup { pane, socket } => {
            let socket = match socket {
                Some(socket) => socket.clone(),
                None => herdr_socket_for(None, here, herdr::sessions)?,
            };
            herdr_at(pane, socket)
        }
    }
}

/// The tmux pane `id`, found as `found`, or if `command` says it runs herdr's
/// client, the herdr pane that has focus behind it.
///
/// herdr's client passes what it is sent on to its focused pane, so the pane
/// a person means when they point at the tmux pane is that one. It is looked
/// through once: a focused herdr pane that runs tmux in turn is refused, as
/// the job would be when it fired (`WrongPane::RunsTmux`).
fn through_tmux(id: &str, found: Located, command: &str) -> Result<Located> {
    if command != "herdr" {
        return Ok(found);
    }
    let runs_herdr =
        |e: anyhow::Error| anyhow!("tmux pane {} ({id}) runs herdr: {e}", found.position);
    let pid = tmux_display(&["-p", "-t", id, "#{pane_pid}"])?;
    let pid: u32 = pid
        .parse()
        .with_context(|| format!("tmux gave tmux pane {id} the process id {pid:?}"))?;
    let argv = herdr_client_argv(&ps(&PROCESS_TABLE)?, pid).map_err(runs_herdr)?;
    let socket = herdr_client_socket(&argv, &herdr::sessions()?).map_err(runs_herdr)?;
    let panes = herdr::list_panes(&socket)?;
    let pane = focused(&panes).map_err(runs_herdr)?.clone();
    let target = HerdrTarget::new(pane.id.as_str(), socket.clone());
    let behind = herdr_located(pane, socket);
    if target
        .foreground()?
        .processes
        .iter()
        .any(ForegroundProcess::is_tmux)
    {
        bail!(
            "{}, which has focus in the herdr in tmux pane {} ({id}), runs tmux in \
             turn, and nudge looks through one multiplexer only. Pass -p with the \
             pane you mean.",
            behind.position,
            found.position
        );
    }
    Ok(behind)
}

/// The herdr pane `pane` on the server at `socket`, or if it runs tmux's
/// client, the tmux pane active behind it.
fn herdr_at(pane: &str, socket: PathBuf) -> Result<Located> {
    let target = HerdrTarget::new(pane, socket.clone());
    let found = match target.get() {
        Ok(found) => found,
        // herdr's code for a pane it does not have (seen 2026-11-19). Any
        // other failure says more as it is.
        Err(e) if format!("{e:#}").contains("pane_not_found") => {
            let spec = TargetSpec::Herdr {
                pane: pane.to_string(),
                socket,
                agent_session: None,
            };
            bail!("no herdr pane {}", label(&spec, &Positions::new()));
        }
        Err(e) => return Err(e),
    };
    through_herdr(&target, herdr_located(found, socket))
}

/// The herdr pane `target`, found as `found`, or if a tmux client is in its
/// foreground, the tmux pane active in that client.
///
/// tmux's client passes what it is sent on to its active pane, so that is the
/// pane a person means. It is looked through once: a tmux pane that runs
/// herdr in turn is refused, as the job would be when it fired
/// (`WrongPane::RunsHerdr`).
fn through_herdr(target: &HerdrTarget, found: Located) -> Result<Located> {
    let foreground = target.foreground()?;
    let clients: Vec<&ForegroundProcess> = foreground
        .processes
        .iter()
        .filter(|p| p.is_tmux())
        .collect();
    let Some(client) = clients.first() else {
        return Ok(found);
    };
    for client in &clients {
        refuse_tmux_socket_flags(&client.argv)
            .map_err(|e| anyhow!("{} runs tmux: {e}", found.position))?;
    }
    // herdr 0.9.5 never reports a pane's tty: its `process-info` handler sets
    // it to nothing. The client's own terminal is the pane's, and `ps` says
    // what that is, on macOS as on Linux.
    let tty = match foreground.tty.as_deref().and_then(tty_device) {
        Some(tty) => tty,
        None => {
            tty_device(&ps(&["-o", "tty=", "-p", &client.pid.to_string()])?).with_context(|| {
                format!(
                    "the tmux client in {} has no terminal, so tmux cannot say which \
                     of its panes is active. Pass -p with the pane you mean.",
                    found.position
                )
            })?
        }
    };
    // Not `display-message -c <tty> -p '#{pane_id}'`: it does not answer for
    // the client `-c` names. On a private tmux server with two sessions and a
    // client attached to each (tmux 3.8, 2026-11-19), it printed the second
    // session's pane for both clients' ttys, and for a tty with no client at
    // all, exiting 0. `list-clients` answers each row in its own client's
    // context.
    let clients = tmux(&["list-clients", "-F", "#{client_tty}\t#{pane_id}"])?;
    let id =
        client_pane(&clients, &tty).map_err(|e| anyhow!("{} runs tmux: {e}", found.position))?;
    let behind = TmuxTarget::new(id.as_str());
    let loc = behind.locate()?;
    if behind.current_command()? == "herdr" {
        bail!(
            "tmux pane {} ({id}), active in the tmux in {}, runs herdr in turn, and \
             nudge looks through one multiplexer only. Pass -p with the pane you mean.",
            loc.position,
            found.position
        );
    }
    Ok(tmux_located(loc))
}

/// The socket of the herdr server a herdr client was started for, from its
/// `argv`: `--session NAME`, `--session=NAME` or `session attach NAME` for a
/// named session, and none of them for the default one.
///
/// `--remote` and `--machine` reach a herdr on another machine over SSH,
/// which nudge does not. Anything else herdr can be running as (`herdr
/// server`, an API command) is no client to look through.
fn herdr_client_socket(argv: &[String], sessions: &[HerdrSession]) -> Result<PathBuf> {
    let mut session = None;
    let mut command = Vec::new();
    let mut rest = argv.iter().skip(1).map(String::as_str);
    while let Some(arg) = rest.next() {
        let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
        match flag {
            "--remote" | "--machine" => bail!(
                "it was started with {flag}, for a herdr on another machine, which \
                 nudge does not reach. Pass -p with the pane you mean."
            ),
            "--session" => {
                session = match arg.split_once('=') {
                    Some((_, name)) => Some(name),
                    None => rest.next(),
                };
                if session.is_none_or(str::is_empty) {
                    bail!("it was given --session with no session's name");
                }
            }
            // herdr's one other option that takes a value.
            "--remote-keybindings" => {
                if !arg.contains('=') {
                    rest.next();
                }
            }
            _ if arg.starts_with('-') => {}
            _ => command.push(arg),
        }
    }
    match command.as_slice() {
        [] => {}
        ["session", "attach", name] => session = Some(name),
        _ => bail!(
            "`{}` is no herdr client. Pass -p with the pane you mean.",
            argv.join(" ")
        ),
    }
    running_session(session, sessions)
}

/// Refuse a tmux client started for a tmux server other than the default
/// one, from its `argv`.
///
/// A tmux job stores a pane's id and no socket, and fires on the default
/// server, where the same id names some other pane or none. Only tmux's own
/// options are read, those before its command: `resize-pane -L` is not a
/// socket. They are read as getopt reads them, so `-uL x` and `-Lx` count, and
/// the value of `-f`, `-c` or `-T` does not.
fn refuse_tmux_socket_flags(argv: &[String]) -> Result<()> {
    let mut rest = argv.iter().skip(1);
    while let Some(arg) = rest.next() {
        let Some(cluster) = arg.strip_prefix('-') else {
            break;
        };
        if cluster.is_empty() || cluster == "-" {
            break;
        }
        for (i, flag) in cluster.char_indices() {
            match flag {
                'L' | 'S' => bail!(
                    "its client was started with -{flag}, for a tmux server other than \
                     the default tmux server. A job keeps no tmux socket, so nudge \
                     reaches only the default tmux server's panes. Pass -p with the pane \
                     you mean."
                ),
                // The option's value is the rest of the cluster, or the next
                // argument if nothing is left of it.
                'c' | 'f' | 'T' => {
                    if i + flag.len_utf8() == cluster.len() {
                        rest.next();
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// The one pane in `panes` with focus. herdr gives focus to at most one pane
/// per server, and to none when it has no workspace yet, as just after it
/// starts. The first pane listed is no stand-in for it.
fn focused(panes: &[HerdrPane]) -> Result<&HerdrPane> {
    let mut focused = panes.iter().filter(|p| p.focused);
    match (focused.next(), focused.next()) {
        (Some(pane), None) => Ok(pane),
        (None, _) => bail!(
            "herdr has no focused pane to send to. Pass -p herdr:<workspace>:<pane> \
             with the pane you mean."
        ),
        (Some(_), Some(_)) => bail!(
            "herdr reports more than one focused pane, so which one you are in cannot \
             be told. Pass -p herdr:<workspace>:<pane> with the pane you mean."
        ),
    }
}

/// The socket of the herdr server a `-p herdr:…` means: the named `session`,
/// or with none named, the herdr nudge is running in, or outside herdr,
/// herdr's default session.
///
/// Only a herdr pane or popup says which herdr nudge is in. A tmux pane can
/// carry `HERDR_SOCKET_PATH` too, when its tmux server was started in a herdr
/// pane, but tmux hands every pane the variables that server started with,
/// which still name that herdr after it has gone.
///
/// Inside herdr without `HERDR_SOCKET_PATH`, which herdr sets in every popup
/// and pane, nudge does not know its session, and every session numbers its
/// panes from `w1:p1`: a session-less pane is refused rather than taken for
/// the default session's.
///
/// `sessions` lists herdr's sessions, and is called only when the answer is
/// not already known, so that a herdr that will not answer does not fail a
/// pane in the session nudge is running in.
fn herdr_socket_for(
    session: Option<&str>,
    here: &Here,
    sessions: impl FnOnce() -> Result<Vec<HerdrSession>>,
) -> Result<PathBuf> {
    if session.is_none() {
        match here {
            Here::HerdrPopup {
                socket: Some(socket),
                ..
            }
            | Here::HerdrSplit {
                socket: Some(socket),
            } => return Ok(socket.clone()),
            Here::HerdrPopup { socket: None, .. } | Here::HerdrSplit { socket: None } => bail!(
                "herdr did not say which session nudge is running in \
                 (HERDR_SOCKET_PATH is not set). Pass -p herdr:<session>:<pane>."
            ),
            _ => {}
        }
    }
    running_session(session, &sessions()?)
}

/// The socket of the herdr session `name`, or with none named, of herdr's
/// default session, if it is running. A session that is not running has no
/// panes to send to.
fn running_session(name: Option<&str>, sessions: &[HerdrSession]) -> Result<PathBuf> {
    let found = match name {
        Some(name) => sessions.iter().find(|s| s.name == name),
        None => sessions.iter().find(|s| s.default),
    };
    match (found, name) {
        (Some(s), _) if s.running => Ok(s.socket.clone()),
        (Some(s), _) => bail!("no running herdr session {}", s.name),
        (None, Some(name)) => bail!("no running herdr session {name}"),
        (None, None) => bail!("no running herdr session: herdr lists no default session"),
    }
}

/// The terminal device in `ps -o tty=`'s answer: `pts/5` on Linux, `ttys005`
/// on macOS, either under `/dev`. `None` for a process with no terminal, which
/// Linux prints as `?` and macOS as `??`.
fn tty_device(ps_tty: &str) -> Option<String> {
    let tty = ps_tty.trim();
    if tty.is_empty() || tty.chars().all(|c| c == '?') {
        return None;
    }
    if tty.starts_with('/') {
        return Some(tty.to_string());
    }
    Some(format!("/dev/{tty}"))
}

/// What `ps` is asked for to find a herdr client's argv: every process, with
/// its parent and its whole command line.
///
/// `-ww`, because `ps` otherwise cuts `args` to fit a width. procps on Linux
/// cuts it to `COLUMNS` whenever that is set, even with its output going to a
/// pipe as it does here (procps-ng 4.0.7, seen 2026-11-19), and a shell can
/// export `COLUMNS` to everything it runs. macOS's `ps` cuts it to the
/// terminal's width when it writes to one. A client's argv cut short can end
/// in the middle of a `--session` name, and a name cut short can be another
/// session's whole name. Both `ps`es take `-ww` as no limit at all.
const PROCESS_TABLE: [&str; 4] = ["-ww", "-A", "-o", "pid=,ppid=,args="];

/// The argv of the herdr client running in a tmux pane whose process is
/// `pane_pid`, from `ps` run with [`PROCESS_TABLE`]: the herdr nearest that
/// process in its tree, the process itself included.
///
/// Nearest, because a client that started herdr's server keeps it as its
/// child (`herdr` over `/usr/bin/herdr server`, seen 2026-11-19), and only the
/// client's argv says which session it is for. Two at the same depth cannot be
/// told apart, and are refused. `ps` joins argv with spaces, which split back the same unless
/// an argument has one in it; a session's name does not.
fn herdr_client_argv(ps: &str, pane_pid: u32) -> Result<Vec<String>> {
    let processes: Vec<(u32, u32, Vec<String>)> = ps
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent = fields.next()?.parse().ok()?;
            Some((pid, parent, fields.map(String::from).collect()))
        })
        .collect();
    let is_herdr = |argv: &[String]| {
        argv.first()
            .is_some_and(|run| Path::new(run).file_name() == Some("herdr".as_ref()))
    };
    // Each process is visited once, so that a table in which a process seems
    // to be its own ancestor cannot keep this going.
    let mut seen = std::collections::HashSet::from([pane_pid]);
    let mut generation: Vec<u32> = vec![pane_pid];
    while !generation.is_empty() {
        let herdrs: Vec<&Vec<String>> = processes
            .iter()
            .filter(|(pid, _, argv)| generation.contains(pid) && is_herdr(argv))
            .map(|(_, _, argv)| argv)
            .collect();
        match herdrs.as_slice() {
            [] => {}
            [argv] => return Ok((*argv).clone()),
            _ => bail!(
                "{} herdr processes sit side by side under the pane's process \
                 {pane_pid}, so which is the client cannot be told. Pass -p with the \
                 pane you mean.",
                herdrs.len()
            ),
        }
        generation = processes
            .iter()
            .filter(|(pid, parent, _)| generation.contains(parent) && seen.insert(*pid))
            .map(|(pid, _, _)| *pid)
            .collect();
    }
    bail!(
        "no herdr process is under the pane's process {pane_pid}. Pass -p with the \
         pane you mean."
    )
}

/// The pane active in the tmux client on the terminal `tty`, from `tmux
/// list-clients -F '#{client_tty}\t#{pane_id}'`, which answers each row for
/// its own client. A row that is not a tty and a pane id is skipped.
fn client_pane(list_clients: &str, tty: &str) -> Result<String> {
    list_clients
        .lines()
        .filter_map(|row| {
            let (client, pane) = row.split_once('\t')?;
            let pane = pane.trim_end();
            (!client.is_empty() && pane.starts_with('%')).then_some((client, pane))
        })
        .find(|(client, _)| *client == tty)
        .map(|(_, pane)| pane.to_string())
        .ok_or_else(|| {
            anyhow!(
                "no client of the default tmux server is on its terminal {tty}, so which \
                 tmux pane it shows cannot be told. Pass -p with the pane you mean."
            )
        })
}

/// What a job stores for the herdr pane `pane` on the server at `socket`, and
/// how to show it. The pane's conversation is the one it holds now.
fn herdr_located(pane: HerdrPane, socket: PathBuf) -> Located {
    let spec = TargetSpec::Herdr {
        pane: pane.id,
        socket,
        agent_session: pane.agent_session,
    };
    Located {
        position: label(&spec, &Positions::new()),
        name: pane.terminal_title.as_deref().and_then(session_name),
        spec,
    }
}

/// What a job stores for a tmux pane as `list-panes` listed it, and how to
/// show it.
fn row_located(row: &Row) -> Located {
    Located {
        spec: TargetSpec::Tmux {
            pane: row.pane_id.clone(),
            server: row.server.clone(),
        },
        position: row.target.clone(),
        name: session_name(&row.title),
    }
}

/// What a job stores for a tmux pane as [`TmuxTarget::locate`] found it, and
/// how to show it.
fn tmux_located(loc: PaneLocation) -> Located {
    Located {
        spec: TargetSpec::Tmux {
            pane: loc.id,
            server: loc.server,
        },
        position: loc.position,
        name: loc.name,
    }
}

/// What `tmux display-message` prints for `args`, which should include `-p`.
fn tmux_display(args: &[&str]) -> Result<String> {
    let mut command = vec!["display-message"];
    command.extend_from_slice(args);
    tmux(&command)
}

/// What `tmux` prints for `args`, on the server nudge's environment names.
fn tmux(args: &[&str]) -> Result<String> {
    let out = Command::new("tmux")
        .args(args)
        .output()
        .context("failed to run tmux")?;
    if !out.status.success() {
        bail!(
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// What `ps` prints for `args`.
fn ps(args: &[&str]) -> Result<String> {
    let out = Command::new("ps")
        .args(args)
        .output()
        .context("failed to run ps")?;
    if !out.status.success() {
        bail!(
            "ps {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            names: HashMap::new(),
            herdr_names: HashMap::new(),
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

    fn herdr(pane: &str, socket: &str) -> TargetSpec {
        TargetSpec::Herdr {
            pane: pane.into(),
            socket: socket.into(),
            agent_session: None,
        }
    }

    #[test]
    fn herdr_session_comes_from_the_socket_path() {
        assert_eq!(
            herdr_session_of(Path::new("/h/.config/herdr/sessions/beaver/herdr.sock")).as_deref(),
            Some("beaver")
        );
        assert_eq!(
            herdr_session_of(Path::new("/h/.config/herdr/herdr.sock")),
            None
        );
    }

    #[test]
    fn herdr_panes_are_labelled_with_their_session() {
        assert_eq!(
            label(
                &herdr("w7:pX", "/h/.config/herdr/herdr.sock"),
                &Positions::new()
            ),
            "herdr:default:w7:pX"
        );
        assert_eq!(
            label(
                &herdr("w1:p2", "/h/.config/herdr/sessions/beaver/herdr.sock"),
                &Positions::new()
            ),
            "herdr:beaver:w1:p2"
        );
    }

    /// A label copied into `-p` from inside another named herdr session means
    /// the pane it was written for, the default session's included: a label
    /// that left the session out would mean the one nudge is running in.
    #[test]
    fn a_default_session_label_as_p_means_the_default_session_from_anywhere() {
        let label = label(&herdr("w7:pX", DEFAULT_SOCKET), &Positions::new());
        assert_eq!(label, "herdr:default:w7:pX");
        let PaneArg::Herdr { session, pane } = crate::here::parse_pane_arg(&label).unwrap() else {
            panic!("not a herdr pane");
        };
        assert_eq!(pane, "w7:pX");
        let in_beaver = Here::HerdrPopup {
            pane: "w1:p1".into(),
            socket: Some(BEAVER_SOCKET.into()),
        };
        assert_eq!(
            herdr_socket_for(session.as_deref(), &in_beaver, || Ok(all_sessions())).unwrap(),
            PathBuf::from(DEFAULT_SOCKET)
        );
    }

    #[test]
    fn a_herdr_pane_is_named_from_its_title() {
        let socket = "/h/.config/herdr/herdr.sock";
        let mut p = Positions::new();
        p.herdr_names
            .insert((socket.into(), "w7:pX".into()), "ra-scan".into());
        assert_eq!(
            named_label(&herdr("w7:pX", socket), &p),
            "ra-scan · herdr:default:w7:pX"
        );
    }

    /// A pane that has closed is no longer listed, and shows as stored.
    #[test]
    fn a_closed_herdr_pane_is_labelled_as_stored() {
        let t = herdr("w7:pX", "/h/.config/herdr/herdr.sock");
        let p = Positions::new();
        assert_eq!(name(&t, &p), None);
        assert_eq!(named_label(&t, &p), "herdr:default:w7:pX");
    }

    // --- finding the pane `-p` and `-a` mean ----------------------------------

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    const DEFAULT_SOCKET: &str = "/h/.config/herdr/herdr.sock";
    const BEAVER_SOCKET: &str = "/h/.config/herdr/sessions/beaver/herdr.sock";

    fn session(name: &str, default: bool, running: bool) -> HerdrSession {
        HerdrSession {
            name: name.into(),
            default,
            running,
            socket: if default {
                DEFAULT_SOCKET.into()
            } else {
                format!("/h/.config/herdr/sessions/{name}/herdr.sock").into()
            },
        }
    }

    /// The default session and `beaver` running, `work` stopped.
    fn all_sessions() -> Vec<HerdrSession> {
        vec![
            session("default", true, true),
            session("beaver", false, true),
            session("work", false, false),
        ]
    }

    fn herdr_pane(id: &str, focused: bool) -> HerdrPane {
        HerdrPane {
            id: id.into(),
            focused,
            agent: None,
            agent_session: None,
            terminal_title: None,
        }
    }

    #[test]
    fn a_herdr_clients_session_comes_from_its_argv() {
        let s = all_sessions();
        assert_eq!(
            herdr_client_socket(&argv(&["herdr"]), &s).unwrap(),
            PathBuf::from(DEFAULT_SOCKET)
        );
        for a in [
            &["herdr", "--session", "beaver"][..],
            &["herdr", "--session=beaver"],
            &["herdr", "session", "attach", "beaver"],
            &["/usr/bin/herdr", "--session", "beaver"],
        ] {
            assert_eq!(
                herdr_client_socket(&argv(a), &s).unwrap(),
                PathBuf::from(BEAVER_SOCKET),
                "{a:?}"
            );
        }
    }

    #[test]
    fn a_herdr_client_on_another_machine_is_refused() {
        for a in [
            &["herdr", "--remote", "host"][..],
            &["herdr", "--remote=host"],
            &["herdr", "--session", "beaver", "--remote", "host"],
        ] {
            let e = herdr_client_socket(&argv(a), &all_sessions())
                .unwrap_err()
                .to_string();
            assert!(e.contains("--remote"), "{a:?}: {e}");
        }
        let e = herdr_client_socket(&argv(&["herdr", "--machine", "box"]), &all_sessions())
            .unwrap_err()
            .to_string();
        assert!(e.contains("--machine"), "{e}");
    }

    #[test]
    fn a_herdr_client_of_a_session_not_running_is_refused() {
        let e = herdr_client_socket(&argv(&["herdr", "--session", "nope"]), &all_sessions())
            .unwrap_err()
            .to_string();
        assert!(e.contains("no running herdr session nope"), "{e}");
        let e = herdr_client_socket(&argv(&["herdr", "--session", "work"]), &all_sessions())
            .unwrap_err()
            .to_string();
        assert!(e.contains("no running herdr session work"), "{e}");
    }

    /// Only an attached client passes keys on to a pane; anything else herdr
    /// runs as is no client to look through.
    #[test]
    fn a_herdr_that_is_no_client_is_refused() {
        for a in [
            &["herdr", "server"][..],
            &["herdr", "--session"],
            &["herdr", "session", "attach"],
        ] {
            assert!(
                herdr_client_socket(&argv(a), &all_sessions()).is_err(),
                "{a:?}"
            );
        }
    }

    #[test]
    fn a_tmux_client_on_the_default_server_is_looked_through() {
        for a in [
            &["tmux"][..],
            &["tmux", "attach"],
            &["tmux", "-u", "attach", "-t", "main"],
            // An option's argument is not an option.
            &["tmux", "-f", "-L.conf", "new-session"],
            // Nor is a command's own flag: `resize-pane -L` is "left".
            &["tmux", "resize-pane", "-L", "5"],
        ] {
            assert!(refuse_tmux_socket_flags(&argv(a)).is_ok(), "{a:?}");
        }
    }

    #[test]
    fn a_tmux_client_on_another_server_is_refused() {
        for a in [
            &["tmux", "-L", "x"][..],
            &["tmux", "-Sx"],
            &["tmux", "-uL", "x"],
            &["tmux", "-f", "/dev/null", "-S", "/s", "attach"],
            &["/usr/bin/tmux", "-Lx", "attach"],
        ] {
            let e = refuse_tmux_socket_flags(&argv(a)).unwrap_err().to_string();
            assert!(e.contains("default tmux server"), "{a:?}: {e}");
        }
    }

    #[test]
    fn the_focused_herdr_pane_is_found() {
        let panes = [herdr_pane("w1:p1", false), herdr_pane("w1:p2", true)];
        assert_eq!(focused(&panes).unwrap().id, "w1:p2");
    }

    /// herdr just started, with no workspace, has no focused pane, and the
    /// first pane listed is no answer.
    #[test]
    fn no_focused_herdr_pane_is_refused() {
        for panes in [
            vec![],
            vec![herdr_pane("w1:p1", false)],
            vec![herdr_pane("w1:p1", true), herdr_pane("w2:p1", true)],
        ] {
            let e = focused(&panes).unwrap_err().to_string();
            assert!(e.contains("-p"), "{panes:?}: {e}");
        }
    }

    #[test]
    fn a_stopped_or_unknown_session_is_refused() {
        let stopped = [session("beaver", false, false)];
        let e = herdr_socket_for(Some("beaver"), &Here::Outside, || Ok(stopped.to_vec()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("no running herdr session beaver"), "{e}");
        let e = herdr_socket_for(Some("nope"), &Here::Outside, || Ok(all_sessions()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("no running herdr session nope"), "{e}");
    }

    #[test]
    fn a_herdr_pane_with_no_session_named_is_on_nudges_herdr_or_the_default() {
        let s = all_sessions();
        let split = Here::HerdrSplit {
            socket: Some("/s".into()),
        };
        assert_eq!(
            herdr_socket_for(None, &split, || Ok(s.clone())).unwrap(),
            PathBuf::from("/s")
        );
        let popup = Here::HerdrPopup {
            pane: "w1:p1".into(),
            socket: Some("/p".into()),
        };
        assert_eq!(
            herdr_socket_for(None, &popup, || Ok(s.clone())).unwrap(),
            PathBuf::from("/p")
        );
        for here in [Here::Outside, Here::TmuxPane { pane: "%3".into() }] {
            assert_eq!(
                herdr_socket_for(None, &here, || Ok(s.clone())).unwrap(),
                PathBuf::from(DEFAULT_SOCKET),
                "{here:?}"
            );
        }
        // A session named outright is that session, wherever nudge runs. The
        // default session too, by the name herdr gives it, which is how the
        // README says to name it from inside another session.
        assert_eq!(
            herdr_socket_for(Some("beaver"), &split, || Ok(s.clone())).unwrap(),
            PathBuf::from(BEAVER_SOCKET)
        );
        assert_eq!(
            herdr_socket_for(Some("default"), &popup, || Ok(s.clone())).unwrap(),
            PathBuf::from(DEFAULT_SOCKET)
        );
    }

    /// herdr sets `HERDR_SOCKET_PATH` in every popup and pane, so without it
    /// nudge does not know its session, and the default session's `w1:p1` is
    /// another pane than this session's: refused, not guessed.
    #[test]
    fn a_herdr_pane_with_no_session_and_no_socket_is_refused_not_guessed() {
        let never = || -> Result<Vec<HerdrSession>> { panic!("asked herdr for its sessions") };
        let popup = Here::HerdrPopup {
            pane: "w1:p1".into(),
            socket: None,
        };
        let split = Here::HerdrSplit { socket: None };
        for here in [&popup, &split] {
            let e = herdr_socket_for(None, here, never).unwrap_err().to_string();
            assert!(
                e.contains("HERDR_SOCKET_PATH") && e.contains("-p herdr:<session>:<pane>"),
                "{e}"
            );
        }
        // `-a` takes the same way out.
        let e = auto_locate(&popup).unwrap_err().to_string();
        assert!(e.contains("HERDR_SOCKET_PATH"), "{e}");
    }

    #[test]
    fn a_herdr_pane_with_a_named_session_and_no_socket_resolves_that_session() {
        let s = all_sessions();
        for here in [
            Here::HerdrPopup {
                pane: "w1:p1".into(),
                socket: None,
            },
            Here::HerdrSplit { socket: None },
        ] {
            assert_eq!(
                herdr_socket_for(Some("beaver"), &here, || Ok(s.clone())).unwrap(),
                PathBuf::from(BEAVER_SOCKET)
            );
            assert_eq!(
                herdr_socket_for(Some("default"), &here, || Ok(s.clone())).unwrap(),
                PathBuf::from(DEFAULT_SOCKET)
            );
        }
    }

    #[test]
    fn no_running_default_session_is_refused() {
        let e = herdr_socket_for(None, &Here::Outside, || {
            Ok(vec![session("default", true, false)])
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("no running herdr session"), "{e}");
        assert!(herdr_socket_for(None, &Here::Outside, || Ok(vec![])).is_err());
    }

    /// herdr is not asked for its sessions when the socket is already known:
    /// a failing or slow `session list` must not fail a `-p herdr:<pane>` from
    /// inside a popup or split.
    #[test]
    fn the_session_list_is_not_asked_for_when_the_socket_is_known() {
        let never = || -> Result<Vec<HerdrSession>> { panic!("asked herdr for its sessions") };
        let popup = Here::HerdrPopup {
            pane: "w1:p1".into(),
            socket: Some("/p".into()),
        };
        let split = Here::HerdrSplit {
            socket: Some("/s".into()),
        };
        assert_eq!(
            herdr_socket_for(None, &popup, never).unwrap(),
            PathBuf::from("/p")
        );
        assert_eq!(
            herdr_socket_for(None, &split, never).unwrap(),
            PathBuf::from("/s")
        );
        // Named outright, it is the list that says where the session is.
        let e = herdr_socket_for(Some("beaver"), &popup, || bail!("herdr: timed out"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("timed out"), "{e}");
    }

    /// Refused before anything is run: neither touches tmux, herdr or ps.
    #[test]
    fn auto_refuses_in_a_herdr_split() {
        let e = auto_locate(&Here::HerdrSplit {
            socket: Some("/s".into()),
        })
        .unwrap_err()
        .to_string();
        assert!(e.contains("herdr popup") && e.contains("-p herdr:"), "{e}");
    }

    #[test]
    fn auto_refuses_outside_tmux_and_herdr() {
        let e = auto_locate(&Here::Outside).unwrap_err().to_string();
        assert!(
            e.contains("tmux") && e.contains("herdr") && e.contains("-p"),
            "{e}"
        );
    }

    /// `ps -o tty=` as Linux and macOS print it.
    #[test]
    fn a_tty_from_ps_is_its_device() {
        assert_eq!(tty_device("pts/16\n").as_deref(), Some("/dev/pts/16"));
        assert_eq!(tty_device("ttys005\n").as_deref(), Some("/dev/ttys005"));
        assert_eq!(tty_device("  pts/5  ").as_deref(), Some("/dev/pts/5"));
        assert_eq!(tty_device("/dev/pts/7").as_deref(), Some("/dev/pts/7"));
        // No terminal: `?` on Linux, `??` on macOS, or nothing at all.
        for none in ["?\n", "??", "", "\n"] {
            assert_eq!(tty_device(none), None, "{none:?}");
        }
    }

    /// `ps -A -o pid=,ppid=,args=` around a tmux pane running herdr, in the
    /// shape it had on 2026-11-19: the pane's shell, the client under it, and
    /// the server the client started under that.
    const PS: &str = "    1     0 /sbin/init
10363  8490 -fish
10962 10363 herdr --session beaver
11012 10962 /usr/bin/herdr server
11020 11012 /bin/zsh
20000     1 herdr
";

    #[test]
    fn the_herdr_client_is_the_herdr_nearest_the_pane() {
        assert_eq!(
            herdr_client_argv(PS, 10363).unwrap(),
            argv(&["herdr", "--session", "beaver"])
        );
        // A pane made to run herdr itself, with no shell.
        assert_eq!(
            herdr_client_argv(PS, 10962).unwrap(),
            argv(&["herdr", "--session", "beaver"])
        );
    }

    #[test]
    fn no_herdr_or_two_under_the_pane_is_refused() {
        assert!(herdr_client_argv(PS, 99999).is_err(), "no such process");
        assert!(herdr_client_argv(PS, 11020).is_err(), "nothing under zsh");
        let two = format!("{PS}10970 10363 herdr\n");
        assert!(herdr_client_argv(&two, 10363).is_err());
    }

    /// `tmux list-clients -F '#{client_tty}\t#{pane_id}'` with two sessions,
    /// one client attached to each, as tmux 3.8 answered on 2026-11-19: each
    /// row in its own client's context.
    const CLIENTS: &str = "/dev/pts/24\t%1\n/dev/pts/25\t%3\n";

    #[test]
    fn each_tmux_client_shows_its_own_pane() {
        assert_eq!(client_pane(CLIENTS, "/dev/pts/24").unwrap(), "%1");
        assert_eq!(client_pane(CLIENTS, "/dev/pts/25").unwrap(), "%3");
    }

    /// No client on the terminal is no answer, whatever other clients show.
    #[test]
    fn a_terminal_with_no_tmux_client_is_refused() {
        for tty in ["/dev/pts/999", "/dev/pts/2", ""] {
            let e = client_pane(CLIENTS, tty).unwrap_err().to_string();
            assert!(e.contains("-p"), "{tty:?}: {e}");
        }
        assert!(client_pane("", "/dev/pts/24").is_err(), "no clients at all");
    }

    #[test]
    fn blank_and_malformed_client_rows_are_skipped() {
        let out =
            "\n/dev/pts/24\n/dev/pts/24\t\n/dev/pts/24\tnot-a-pane\n\t%9\n  \n/dev/pts/24\t%1\n";
        assert_eq!(client_pane(out, "/dev/pts/24").unwrap(), "%1");
        assert!(client_pane("\t%9\n", "").is_err(), "a row with no tty");
    }

    #[test]
    fn a_herdr_pane_is_located_by_its_label_and_title() {
        let pane = HerdrPane {
            id: "w1:p2".into(),
            focused: true,
            agent: Some("claude".into()),
            agent_session: Some("abc".into()),
            terminal_title: Some("✳ ra-scan".into()),
        };
        assert_eq!(
            herdr_located(pane, BEAVER_SOCKET.into()),
            Located {
                spec: TargetSpec::Herdr {
                    pane: "w1:p2".into(),
                    socket: BEAVER_SOCKET.into(),
                    agent_session: Some("abc".into()),
                },
                position: "herdr:beaver:w1:p2".into(),
                name: Some("ra-scan".into()),
            }
        );
        let plain = herdr_located(herdr_pane("w7:pX", true), DEFAULT_SOCKET.into());
        assert_eq!(plain.position, "herdr:default:w7:pX");
        assert_eq!(plain.name, None);
    }

    #[test]
    fn a_tmux_row_is_located_by_its_id_and_position() {
        let row = Row {
            server: Some("1793226261-4820".into()),
            pane_id: "%5".into(),
            window_id: "@2".into(),
            is_last: true,
            target: "main:2.0".into(),
            command: "claude".into(),
            title: "✳ ra-scan".into(),
        };
        assert_eq!(
            row_located(&row),
            Located {
                spec: TargetSpec::Tmux {
                    pane: "%5".into(),
                    server: Some("1793226261-4820".into()),
                },
                position: "main:2.0".into(),
                name: Some("ra-scan".into()),
            }
        );
    }

    // --- the panes the picker offers -------------------------------------------

    fn row(pane_id: &str, target: &str, command: &str) -> Row {
        Row {
            server: Some("100-1".into()),
            pane_id: pane_id.into(),
            window_id: "@1".into(),
            is_last: false,
            target: target.into(),
            command: command.into(),
            title: String::new(),
        }
    }

    /// What tells a listed pane from the others: tmux's id for it, or a herdr
    /// pane's label.
    fn key(pane: &Pane) -> &str {
        match &pane.spec {
            TargetSpec::Tmux { pane, .. } => pane,
            TargetSpec::Herdr { .. } => &pane.target,
        }
    }

    fn ids(panes: &[Pane]) -> Vec<&str> {
        panes.iter().map(key).collect()
    }

    /// A command line longer than any terminal is wide comes back whole,
    /// whatever width `ps` would otherwise cut it to.
    #[test]
    fn the_process_table_keeps_a_whole_command_line() {
        let long = format!("--session={}", "x".repeat(400));
        // The shell waits in its own `read` builtin, for a line that never
        // comes, so the long argument stays in a running process's command
        // line, and killing it leaves no child of its own running behind.
        let mut child = Command::new("sh")
            .args(["-c", "read line", &long])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let out = Command::new("ps")
            .args(PROCESS_TABLE)
            .env("COLUMNS", "80")
            .output()
            .unwrap();
        let _ = child.kill();
        let _ = child.wait();
        let table = String::from_utf8_lossy(&out.stdout);
        let line = table
            .lines()
            .find(|l| l.split_whitespace().next() == Some(child.id().to_string().as_str()))
            .expect("ps lists the child");
        assert!(line.contains(&long), "cut short: {line:?}");
    }

    fn herdr_job(pane: &str) -> TargetSpec {
        TargetSpec::Herdr {
            pane: pane.into(),
            socket: PathBuf::from("/run/test/herdr.sock"),
            agent_session: None,
        }
    }

    /// The daemon labels a batch of tmux jobs without asking herdr, which
    /// may have stopped answering.
    #[test]
    fn labelling_only_tmux_jobs_never_asks_herdr() {
        let jobs = [tmux("%3", Some("100-1")), tmux("%4", Some("100-1"))];
        let positions = positions_with(
            &jobs,
            || Ok(vec![row("%3", "main:2.0", "claude")]),
            || panic!("herdr was asked to label tmux jobs"),
        );
        assert_eq!(
            positions.panes.get("%3").map(String::as_str),
            Some("main:2.0")
        );
        assert!(positions.herdr_names.is_empty());
    }

    /// One herdr job in the batch is reason enough to ask.
    #[test]
    fn labelling_a_herdr_job_asks_herdr() {
        let jobs = [tmux("%3", Some("100-1")), herdr_job("w1:p1")];
        let key = (PathBuf::from("/run/test/herdr.sock"), "w1:p1".to_string());
        let positions = positions_with(
            &jobs,
            || Ok(vec![row("%3", "main:2.0", "claude")]),
            || HashMap::from([(key.clone(), "ra-scan-3".to_string())]),
        );
        assert_eq!(
            name(&jobs[1], &positions),
            Some("ra-scan-3"),
            "the herdr job is named"
        );
        assert_eq!(
            positions.panes.get("%3").map(String::as_str),
            Some("main:2.0"),
            "tmux's half is still there"
        );
    }

    fn running_tmux(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    /// herdr's client passes what it is typed on to whichever herdr pane has
    /// focus, so its tmux pane is no place to send a nudge.
    #[test]
    fn a_tmux_pane_running_herdr_is_not_offered() {
        let panes = merge(
            Ok(vec![
                row("%1", "main:1.0", "fish"),
                row("%3", "main:2.0", "herdr"),
            ]),
            vec![],
        );
        assert_eq!(ids(&panes), ["%1"]);
        assert_eq!(
            panes[0],
            Pane {
                target: "main:1.0".into(),
                title: String::new(),
                spec: tmux("%1", Some("100-1")),
            }
        );
    }

    /// tmux's client passes what it is typed on to its active pane, which the
    /// tmux half already lists.
    #[test]
    fn a_herdr_pane_running_tmux_is_not_offered() {
        let panes = merge(
            Ok(vec![]),
            vec![(
                session("default", true, true),
                vec![herdr_pane("w1:p1", false), herdr_pane("w1:p2", true)],
                running_tmux(&["w1:p2"]),
            )],
        );
        assert_eq!(ids(&panes), ["herdr:default:w1:p1"]);
    }

    /// herdr not installed, or with no session running, has nothing to add,
    /// and that is no reason to lose tmux's panes.
    #[test]
    fn herdr_listing_failure_leaves_the_tmux_panes() {
        let panes = merge(
            Ok(vec![
                row("%1", "main:1.0", "fish"),
                row("%2", "main:1.1", "claude"),
            ]),
            vec![],
        );
        assert_eq!(ids(&panes), ["%1", "%2"]);
    }

    /// Nor is tmux not running a reason to lose herdr's.
    #[test]
    fn tmux_listing_failure_leaves_the_herdr_panes() {
        let panes = merge(
            Err(anyhow!("tmux list-panes failed: no server running")),
            vec![(
                session("default", true, true),
                vec![herdr_pane("w1:p1", true)],
                HashSet::new(),
            )],
        );
        assert_eq!(ids(&panes), ["herdr:default:w1:p1"]);
    }

    /// A herdr pane is offered by its label, and what a job stores for it
    /// carries the conversation it holds as listed, as `-p` stamps one.
    #[test]
    fn herdr_panes_from_a_named_session_carry_its_socket() {
        let pane = HerdrPane {
            id: "w1:p1".into(),
            focused: false,
            agent: Some("claude".into()),
            agent_session: Some("abc".into()),
            terminal_title: Some("✳ ra-scan".into()),
        };
        let panes = merge(
            Ok(vec![]),
            vec![(session("beaver", false, true), vec![pane], HashSet::new())],
        );
        assert_eq!(
            panes,
            [Pane {
                target: "herdr:beaver:w1:p1".into(),
                title: "✳ ra-scan".into(),
                spec: TargetSpec::Herdr {
                    pane: "w1:p1".into(),
                    socket: BEAVER_SOCKET.into(),
                    agent_session: Some("abc".into()),
                },
            }]
        );
    }

    /// The pane `-a` would find is preselected, whatever either was stamped
    /// with; the first is, when `-a` finds none or one not listed.
    #[test]
    fn the_preselected_pane_is_the_one_auto_finds_else_the_first() {
        let panes = merge(
            Ok(vec![
                row("%1", "main:1.0", "fish"),
                row("%2", "main:1.1", "claude"),
            ]),
            vec![(
                session("default", true, true),
                vec![herdr_pane("w1:p1", true)],
                HashSet::new(),
            )],
        );
        let herdr_found = TargetSpec::Herdr {
            pane: "w1:p1".into(),
            socket: DEFAULT_SOCKET.into(),
            agent_session: Some("abc".into()),
        };
        assert_eq!(default_idx(&panes, Some(&herdr_found)), 2);
        assert_eq!(default_idx(&panes, Some(&tmux("%2", None))), 1);
        assert_eq!(default_idx(&panes, Some(&tmux("%9", None))), 0);
        assert_eq!(default_idx(&panes, None), 0);
    }

    fn foreground(names: &[&str]) -> Foreground {
        Foreground {
            tty: None,
            processes: names
                .iter()
                .map(|name| ForegroundProcess {
                    name: (*name).into(),
                    argv: vec![name.split(':').next().unwrap_or(name).into()],
                    pid: 7,
                })
                .collect(),
        }
    }

    /// Each running session is asked for its panes, and each pane for what
    /// runs in it. A Linux tmux client calls itself `tmux: client`, which is
    /// the name herdr reports for it.
    #[test]
    fn the_herdr_half_marks_the_panes_running_tmux() {
        let listing = herdr_listing(
            vec![
                session("default", true, true),
                session("work", false, false),
            ],
            |socket| {
                assert_eq!(socket, Path::new(DEFAULT_SOCKET), "a stopped session");
                Ok(vec![herdr_pane("w1:p1", true), herdr_pane("w1:p2", false)])
            },
            |_, pane| match pane {
                "w1:p2" => Ok(foreground(&["tmux: client"])),
                _ => Ok(foreground(&["fish", "claude"])),
            },
        );
        assert_eq!(listing.len(), 1);
        let (session, panes, runs_tmux) = &listing[0];
        assert_eq!(session.name, "default");
        assert_eq!(panes.len(), 2);
        assert_eq!(runs_tmux, &running_tmux(&["w1:p2"]));
    }

    /// A session that will not list its panes is left out whole; the other
    /// sessions are still offered.
    #[test]
    fn a_herdr_session_that_will_not_list_its_panes_is_left_out() {
        let listing = herdr_listing(
            all_sessions(),
            |socket| {
                if socket == Path::new(BEAVER_SOCKET) {
                    bail!("herdr: server_unreachable")
                }
                Ok(vec![herdr_pane("w1:p1", true)])
            },
            |_, _| Ok(foreground(&["fish"])),
        );
        let names: Vec<&str> = listing.iter().map(|(s, ..)| s.name.as_str()).collect();
        assert_eq!(names, ["default"]);
    }

    /// A pane that closes between `pane list` and its `process-info` is left
    /// out alone: the session's other panes are still offered, with the ones
    /// that run tmux still marked.
    #[test]
    fn a_pane_herdr_will_not_describe_is_left_out_alone() {
        let listing = herdr_listing(
            vec![session("default", true, true)],
            |_| {
                Ok(vec![
                    herdr_pane("w1:p1", true),
                    herdr_pane("w1:p2", false),
                    herdr_pane("w1:p3", false),
                ])
            },
            |_, pane| match pane {
                "w1:p2" => bail!("herdr: pane_not_found"),
                "w1:p3" => Ok(foreground(&["tmux: client"])),
                _ => Ok(foreground(&["fish"])),
            },
        );
        assert_eq!(listing.len(), 1);
        let (_, panes, runs_tmux) = &listing[0];
        let ids: Vec<&str> = panes.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["w1:p1", "w1:p3"]);
        assert_eq!(runs_tmux, &running_tmux(&["w1:p3"]));
    }
}
