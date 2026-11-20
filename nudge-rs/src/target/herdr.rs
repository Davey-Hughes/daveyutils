//! A herdr pane as an injection `Target`, via herdr's CLI: `pane read`,
//! `pane send-text`, `pane send-keys`.
//!
//! Every call but `herdr session list` names the server it is for with
//! `HERDR_SOCKET_PATH`, since a job outlives the shell that scheduled it and
//! the daemon has no herdr of its own to inherit.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{KEYPRESS_PAUSE, Key, PaneDims, Target, WrongPane};

/// A `herdr` command aimed at the server listening on `socket`, and at
/// nothing else.
///
/// Every inherited `HERDR_*` is removed first. They describe whichever herdr
/// nudge was started from, which need not be the server at `socket`: a popup
/// has `HERDR_ACTIVE_PANE_ID` and `HERDR_SESSION`, a pane `HERDR_PANE_ID`,
/// and herdr reads at least the pane ids for `--current` (seen 2026-11-19).
/// One a later herdr reads to pick its server could send a job to another
/// session's panes, so none is kept, known or not.
pub fn herdr(socket: &Path) -> Command {
    // `vars_os`, not `vars`: `vars` panics if any variable in nudge's
    // environment is not UTF-8.
    herdr_with_inherited(socket, std::env::vars_os().map(|(key, _)| key))
}

/// [`herdr`], given the names of the variables it would inherit.
fn herdr_with_inherited(
    socket: &Path,
    inherited: impl IntoIterator<Item = std::ffi::OsString>,
) -> Command {
    let mut cmd = Command::new("herdr");
    for key in inherited {
        if key.to_str().is_some_and(|k| k.starts_with("HERDR_")) {
            cmd.env_remove(key);
        }
    }
    cmd.env("HERDR_SOCKET_PATH", socket);
    cmd
}

/// herdr's name for `key`, as `pane send-keys` takes it.
///
/// Checked against herdr 0.9.5 on 2026-11-19: `esc` and `backspace` are its
/// names, and a bare letter is that letter. It refuses a name it does not
/// know, tmux's `BSpace` among them, with `invalid_key` before it writes
/// anything, so a wrong name here fails loudly rather than typing something.
pub fn herdr_key_name(key: Key) -> String {
    match key {
        Key::Char(c) => c.to_string(),
        Key::Escape => "esc".to_string(),
        Key::Backspace => "backspace".to_string(),
    }
}

/// A pane as `herdr pane list` and `herdr pane get` describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrPane {
    /// herdr's id for the pane, e.g. `w1:p2`.
    pub id: String,
    /// Whether the pane has focus in its server.
    pub focused: bool,
    /// The agent herdr sees running in it: `claude`, `codex`.
    pub agent: Option<String>,
    /// The id of the agent's conversation, as the agent reported it to herdr.
    pub agent_session: Option<String>,
    /// What the program in the pane set as the terminal's title.
    pub terminal_title: Option<String>,
}

/// A herdr session, as `herdr session list --json` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrSession {
    pub name: String,
    /// Whether this is the session a bare `herdr` attaches to.
    pub default: bool,
    pub running: bool,
    /// Its server's API socket.
    pub socket: PathBuf,
}

/// The processes in a pane's foreground, as `herdr pane process-info` lists
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foreground {
    /// The pane's terminal device. herdr 0.9.5 never fills this in: its
    /// `process-info` handler sets it to nothing, and no pane on a live server
    /// had one (2026-11-19). A process's [`pid`](ForegroundProcess::pid) is
    /// what finds it instead.
    pub tty: Option<String>,
    pub processes: Vec<ForegroundProcess>,
}

/// One process in a pane's foreground.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProcess {
    /// The name the kernel gives it, after the file it ran: `tmux`, `claude`.
    pub name: String,
    pub argv: Vec<String>,
    /// Kept because herdr reports no tty: the system can say which terminal a
    /// process is on, which is how a tmux client in the pane is found.
    pub pid: u32,
}

impl ForegroundProcess {
    /// Whether this is tmux: in a pane's foreground, its client.
    ///
    /// Not just by name. tmux renames its client `tmux: client` (on Linux it
    /// sets the kernel's name for the process), and that is the name herdr
    /// 0.9.5 reports for it (seen 2026-11-19). Where tmux cannot rename itself
    /// the name is `tmux`, and whatever it goes by, the program run was tmux.
    pub fn is_tmux(&self) -> bool {
        self.name == "tmux"
            || self.name.starts_with("tmux:")
            || self
                .argv
                .first()
                .is_some_and(|run| Path::new(run).file_name() == Some("tmux".as_ref()))
    }
}

/// What herdr answers a request with: the result, or what went wrong.
#[derive(Deserialize)]
struct Response<T> {
    result: Option<T>,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    code: String,
    message: String,
}

#[derive(Deserialize)]
struct RawPane {
    pane_id: String,
    #[serde(default)]
    focused: bool,
    agent: Option<String>,
    agent_session: Option<RawAgentSession>,
    terminal_title: Option<String>,
}

/// herdr describes a session as `{"agent", "kind", "source", "value"}`.
/// The `value` is the agent's own id for it, and the part a job keeps.
#[derive(Deserialize)]
struct RawAgentSession {
    value: String,
}

impl From<RawPane> for HerdrPane {
    fn from(p: RawPane) -> Self {
        HerdrPane {
            id: p.pane_id,
            focused: p.focused,
            agent: p.agent,
            agent_session: p.agent_session.map(|s| s.value),
            terminal_title: p.terminal_title,
        }
    }
}

#[derive(Deserialize)]
struct PaneList {
    panes: Vec<RawPane>,
}

#[derive(Deserialize)]
struct PaneGet {
    pane: RawPane,
}

#[derive(Deserialize)]
struct LayoutResult {
    layout: Layout,
}

#[derive(Deserialize)]
struct Layout {
    panes: Vec<LayoutPane>,
    /// Whether a pane in the tab is zoomed. Required: a layout that does not
    /// say cannot be trusted with a size, and failing to read it gives `None`.
    zoomed: bool,
}

#[derive(Deserialize)]
struct LayoutPane {
    pane_id: String,
    rect: Rect,
}

#[derive(Deserialize)]
struct Rect {
    width: u16,
    height: u16,
}

#[derive(Deserialize)]
struct ProcessInfoResult {
    process_info: ProcessInfo,
}

#[derive(Deserialize)]
struct ProcessInfo {
    tty: Option<String>,
    #[serde(default)]
    foreground_processes: Vec<RawProcess>,
}

#[derive(Deserialize)]
struct RawProcess {
    name: String,
    /// herdr's schema allows `null` here as well as leaving it out.
    argv: Option<Vec<String>>,
    pid: u32,
}

#[derive(Deserialize)]
struct SessionList {
    sessions: Vec<RawSession>,
}

#[derive(Deserialize)]
struct RawSession {
    name: String,
    default: bool,
    running: bool,
    socket_path: PathBuf,
}

/// The `result` of a herdr response, or an error that says what herdr said
/// was wrong.
fn result<T: DeserializeOwned>(json: &str) -> Result<T> {
    let response: Response<T> =
        serde_json::from_str(json).context("herdr answered with something other than JSON")?;
    if let Some(e) = response.error {
        bail!("herdr: {}: {}", e.code, e.message);
    }
    response.result.context("herdr answered with no result")
}

/// The panes in `herdr pane list`'s answer.
pub fn parse_pane_list(json: &str) -> Result<Vec<HerdrPane>> {
    Ok(result::<PaneList>(json)?
        .panes
        .into_iter()
        .map(HerdrPane::from)
        .collect())
}

/// The pane in `herdr pane get`'s answer.
pub fn parse_pane_get(json: &str) -> Result<HerdrPane> {
    Ok(result::<PaneGet>(json)?.pane.into())
}

/// `pane`'s size in `herdr pane layout`'s answer, or `None` if it is not
/// there, its tab is zoomed, or the answer cannot be read: not knowing the
/// size is never a reason to fail (see [`Target::dims`]).
pub fn parse_layout_dims(json: &str, pane: &str) -> Option<PaneDims> {
    let layout = result::<LayoutResult>(json).ok()?.layout;
    // herdr computes each pane's rect from the split tree alone and reports a
    // zoom only beside it: zooming one of two side-by-side panes left both
    // rects at half the width, with `zoomed: true` (seen 2026-11-19). A
    // zoomed pane is drawn over the whole tab. Should its terminal be resized
    // to match, a zoom or unzoom between schedule and fire would reflow the
    // screen while the rect stayed the same, and `--verify` would read the
    // reflow as someone typing and skip the nudge. So in a zoomed tab no size
    // is known, the zoomed pane's or any other's, which sends `--verify` back
    // to the banner check.
    if layout.zoomed {
        return None;
    }
    let rect = layout.panes.into_iter().find(|p| p.pane_id == pane)?.rect;
    Some(PaneDims {
        width: rect.width,
        height: rect.height,
    })
}

/// The foreground processes in `herdr pane process-info`'s answer.
pub fn parse_foreground(json: &str) -> Result<Foreground> {
    let info = result::<ProcessInfoResult>(json)?.process_info;
    Ok(Foreground {
        tty: info.tty,
        processes: info
            .foreground_processes
            .into_iter()
            .map(|p| ForegroundProcess {
                name: p.name,
                argv: p.argv.unwrap_or_default(),
                pid: p.pid,
            })
            .collect(),
    })
}

/// The sessions in `herdr session list --json`'s answer, which, unlike the
/// API's, is not wrapped in a `result`.
pub fn parse_sessions(json: &str) -> Result<Vec<HerdrSession>> {
    let list: SessionList = serde_json::from_str(json)
        .context("herdr listed its sessions as something other than JSON")?;
    Ok(list
        .sessions
        .into_iter()
        .map(|s| HerdrSession {
            name: s.name,
            default: s.default,
            running: s.running,
            socket: s.socket_path,
        })
        .collect())
}

/// Every herdr session on this machine, running or not.
///
/// No `HERDR_SOCKET_PATH`: the sessions are read from herdr's config
/// directory, not asked of a server.
pub fn sessions() -> Result<Vec<HerdrSession>> {
    let out = checked(Command::new("herdr").args(["session", "list", "--json"]))?;
    parse_sessions(&String::from_utf8_lossy(&out.stdout))
}

/// Every pane on the server at `socket`.
pub fn list_panes(socket: &Path) -> Result<Vec<HerdrPane>> {
    let out = checked(herdr(socket).args(["pane", "list"]))?;
    parse_pane_list(&String::from_utf8_lossy(&out.stdout))
}

/// How long nudge waits for any one herdr command before giving up on it.
///
/// herdr's CLI asks its server over a socket and waits for the answer, with
/// no timeout of its own, so a server that has hung keeps every command
/// waiting for as long as it stays hung. nudge cannot wait that long: the
/// daemon fires its jobs one after another, tmux's among them, the dashboard
/// lists panes every two seconds, and `--list` and the picker list them
/// before they show anything. A well herdr answers in milliseconds, so five
/// seconds is a busy machine's worth of slack; past it, the command fails as
/// a herdr that is not running does, which leaves a job unsent and a session
/// out of a listing rather than everything stopped behind it.
const HERDR_TIMEOUT: Duration = Duration::from_secs(5);

/// Run `cmd`, failing with what herdr said on stderr if it does not succeed
/// or does not finish within [`HERDR_TIMEOUT`]. herdr writes a request it
/// refused there as JSON, with the error's code and message.
fn checked(cmd: &mut Command) -> Result<Output> {
    let args: Vec<_> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let out = output_within(cmd, HERDR_TIMEOUT)
        .with_context(|| format!("herdr {args:?} could not be run"))?
        .ok_or_else(|| {
            anyhow!(
                "herdr {args:?} did not answer within {} seconds",
                HERDR_TIMEOUT.as_secs()
            )
        })?;
    if !out.status.success() {
        bail!(
            "herdr {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out)
}

/// What `cmd` printed and how it exited, as [`Command::output`] gives it, or
/// `None` if it was still running `limit` after it started. Then it is
/// killed, and waited for so that it leaves no zombie.
///
/// Each pipe is read on a thread of its own while this waits: a command
/// whose output fills its pipe stops until someone reads it, so waiting for
/// its exit first, or reading one pipe to its end before the other, can wait
/// forever on a command that is not hung at all. Its output is waited for
/// against the same deadline, since whatever the command left running with
/// the pipes open can hold them past its own exit.
fn output_within(cmd: &mut Command, limit: Duration) -> std::io::Result<Option<Output>> {
    let deadline = Instant::now() + limit;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = read_to_end(child.stdout.take());
    let stderr = read_to_end(child.stderr.take());
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let (Ok(stdout), Ok(stderr)) = (
        stdout.recv_timeout(remaining()),
        stderr.recv_timeout(remaining()),
    ) else {
        return Ok(None);
    };
    Ok(Some(Output {
        status,
        stdout,
        stderr,
    }))
}

/// Everything `pipe` holds until it closes, read on a thread of its own and
/// handed back once it has closed. No pipe is nothing.
fn read_to_end(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    match pipe {
        Some(mut pipe) => {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                // A read that fails part way keeps what came before it, as
                // `Command::output` does not; either way the exit status says
                // whether the command did what was asked.
                let _ = pipe.read_to_end(&mut buf);
                let _ = tx.send(buf);
            });
        }
        None => {
            let _ = tx.send(Vec::new());
        }
    }
    rx
}

/// A specific herdr pane, by herdr's id for it (`w1:p2`), on the server
/// listening on `socket`.
pub struct HerdrTarget {
    pane: String,
    socket: PathBuf,
    /// The conversation the pane held when the job was scheduled, if it held
    /// one.
    agent_session: Option<String>,
}

impl HerdrTarget {
    pub fn new(pane: impl Into<String>, socket: impl Into<PathBuf>) -> Self {
        Self {
            pane: pane.into(),
            socket: socket.into(),
            agent_session: None,
        }
    }

    /// Expect the pane to hold `agent_session`, so that
    /// [`Target::wrong_pane`] finds it holding any other. With `None` there
    /// is nothing to compare.
    #[must_use]
    pub fn expecting_session(mut self, agent_session: Option<String>) -> Self {
        self.agent_session = agent_session;
        self
    }

    /// The pane as herdr describes it now.
    pub fn get(&self) -> Result<HerdrPane> {
        let out = self.run(&["pane", "get", &self.pane])?;
        parse_pane_get(&String::from_utf8_lossy(&out.stdout))
    }

    /// The processes in the pane's foreground now.
    pub fn foreground(&self) -> Result<Foreground> {
        let out = self.run(&["pane", "process-info", "--pane", &self.pane])?;
        parse_foreground(&String::from_utf8_lossy(&out.stdout))
    }

    /// Run a herdr subcommand against this pane's server, erroring on a
    /// non-zero exit.
    fn run(&self, args: &[&str]) -> Result<Output> {
        checked(herdr(&self.socket).args(args))
    }

    /// Read what the pane shows now (`--source visible`, not its scrollback),
    /// with `options` added.
    fn read(&self, options: &[&str]) -> Result<String> {
        let mut args = vec!["pane", "read", &self.pane, "--source", "visible"];
        args.extend_from_slice(options);
        let out = self.run(&args)?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl Target for HerdrTarget {
    fn capture(&self) -> Result<String> {
        self.read(&[])
    }

    fn capture_styled(&self) -> Result<String> {
        // herdr keeps SGR in `--format ansi`, re-encoding colours as it goes
        // (`31` comes back as `38;5;1`), and ends every line with CRLF where
        // its text format and tmux's `capture-pane -e` end them with LF (both
        // seen 2026-11-19). The screen readers split lines on LF only, so a CR
        // left in would end every line they read.
        Ok(self.read(&["--format", "ansi"])?.replace("\r\n", "\n"))
    }

    fn send_line(&self, text: &str) -> Result<()> {
        // No `--` before the text: herdr's `send-text` types every argument
        // after the pane id, `--` included, joined with spaces, and takes no
        // options there, so text that starts with a dash (`-v`, `--help`)
        // arrives as it is (seen 2026-11-19).
        self.run(&["pane", "send-text", &self.pane, text])?;
        std::thread::sleep(KEYPRESS_PAUSE);
        self.run(&["pane", "send-keys", &self.pane, "enter"])?;
        // And after it, so a capture taken next shows what the Enter did.
        std::thread::sleep(KEYPRESS_PAUSE);
        Ok(())
    }

    fn send_key(&self, key: Key) -> Result<()> {
        // The pause comes after the key, so that the caller's next send cannot
        // run into it.
        self.run(&["pane", "send-keys", &self.pane, &herdr_key_name(key)])?;
        std::thread::sleep(KEYPRESS_PAUSE);
        Ok(())
    }

    fn wrong_pane(&self) -> Result<Option<WrongPane>> {
        // herdr restores its panes, ids and sessions included, when its server
        // restarts (seen 2026-11-19), so the stamp still matches after a
        // reboot. What it catches is the conversation changing under the id:
        // the agent quit and another started in the pane, or a server killed
        // before it saved its session came back without the pane, and the id
        // went to a new one.
        if let Some(expected) = &self.agent_session
            && let Some(wrong) = check_conversation(expected, self.get()?.agent_session.as_deref())?
        {
            return Ok(Some(wrong));
        }
        // tmux's client passes what it is sent on to whichever tmux pane is
        // active, so a nudge typed into the herdr pane holding it could land
        // in any of them.
        if self
            .foreground()?
            .processes
            .iter()
            .any(ForegroundProcess::is_tmux)
        {
            return Ok(Some(WrongPane::RunsTmux));
        }
        Ok(None)
    }

    fn settle(&self) {
        std::thread::sleep(KEYPRESS_PAUSE);
    }

    /// Ask herdr for the pane's size: the capture cannot say how wide the
    /// pane is (see the tmux backend's `dims`).
    fn dims(&self) -> Option<PaneDims> {
        let out = self.run(&["pane", "layout", "--pane", &self.pane]).ok()?;
        parse_layout_dims(&String::from_utf8_lossy(&out.stdout), &self.pane)
    }
}

/// Whether a pane stamped with the conversation `expected` still holds it,
/// given the one herdr `reported` for it now.
///
/// herdr reporting no conversation says it cannot tell which one the pane
/// holds, not that it holds another: the agent may not have reported one yet,
/// or herdr may have lost it. That is an `Err`, which a job with retries left
/// tries again, and not a final [`WrongPane::OtherConversation`].
fn check_conversation(expected: &str, reported: Option<&str>) -> Result<Option<WrongPane>> {
    match reported {
        Some(reported) if reported == expected => Ok(None),
        Some(_) => Ok(Some(WrongPane::OtherConversation)),
        None => bail!(
            "herdr reports no agent session for the pane, so nudge cannot tell \
             whether it still holds the job's conversation"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Key;

    #[test]
    fn the_same_conversation_is_not_a_wrong_pane() {
        assert_eq!(check_conversation("c1", Some("c1")).unwrap(), None);
    }

    #[test]
    fn another_conversation_is_a_wrong_pane() {
        assert_eq!(
            check_conversation("c1", Some("c2")).unwrap(),
            Some(WrongPane::OtherConversation)
        );
    }

    /// herdr not saying which conversation the pane holds is no proof that it
    /// holds another, so it is an error a retry can recover from.
    #[test]
    fn no_reported_conversation_is_an_error_not_a_wrong_pane() {
        let e = check_conversation("c1", None).unwrap_err().to_string();
        assert!(e.contains("no agent session"), "{e}");
    }

    /// `herdr pane list` from a live server, trimmed to three panes, with
    /// every path, title and session id replaced.
    const PANE_LIST: &str = include_str!("../../tests/fixtures/herdr/pane-list.json");
    const PANE_GET: &str = include_str!("../../tests/fixtures/herdr/pane-get.json");
    const LAYOUT: &str = include_str!("../../tests/fixtures/herdr/pane-layout.json");
    /// The layout fixture split in two and zoomed, in the shape a private
    /// server answered with on 2026-11-19.
    const LAYOUT_ZOOMED: &str = include_str!("../../tests/fixtures/herdr/pane-layout-zoomed.json");
    const PROCESS_INFO: &str = include_str!("../../tests/fixtures/herdr/process-info.json");
    const SESSIONS: &str = include_str!("../../tests/fixtures/herdr/session-list.json");

    #[test]
    fn parses_a_pane_list() {
        let p = parse_pane_list(PANE_LIST).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p.iter().filter(|p| p.focused).count(), 1);
        assert!(p[0].agent_session.is_some());
    }

    #[test]
    fn parses_one_pane() {
        assert_eq!(
            parse_pane_get(PANE_GET).unwrap().agent.as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn reads_a_panes_size_from_the_layout() {
        assert_eq!(
            parse_layout_dims(LAYOUT, "w24:p1"),
            Some(PaneDims {
                width: 126,
                height: 37
            })
        );
        assert_eq!(parse_layout_dims(LAYOUT, "w99:p99"), None);
    }

    /// herdr's rects ignore a zoom, so in a zoomed tab no pane's size is known,
    /// the zoomed pane's or any other's.
    #[test]
    fn a_zoomed_tab_has_no_known_sizes() {
        assert_eq!(parse_layout_dims(LAYOUT_ZOOMED, "w24:p1"), None);
        assert_eq!(parse_layout_dims(LAYOUT_ZOOMED, "w24:p2"), None);
        let unzoomed = LAYOUT_ZOOMED.replace(r#""zoomed": true"#, r#""zoomed": false"#);
        assert_eq!(
            parse_layout_dims(&unzoomed, "w24:p2"),
            Some(PaneDims {
                width: 63,
                height: 37
            })
        );
    }

    /// Every inherited `HERDR_*` goes, whatever it is, and the socket is the
    /// one given.
    #[test]
    fn a_herdr_command_keeps_no_inherited_herdr_variable() {
        let inherited = [
            "HERDR_SESSION",
            "HERDR_PANE_ID",
            "HERDR_ACTIVE_PANE_ID",
            "HERDR_SOCKET_PATH",
            "HERDR_SOMETHING_NEW",
            "PATH",
        ]
        .map(std::ffi::OsString::from);
        let cmd = herdr_with_inherited(Path::new("/run/test/herdr.sock"), inherited);
        let envs: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        for removed in [
            "HERDR_SESSION",
            "HERDR_PANE_ID",
            "HERDR_ACTIVE_PANE_ID",
            "HERDR_SOMETHING_NEW",
        ] {
            assert_eq!(
                envs.get(std::ffi::OsStr::new(removed)),
                Some(&None),
                "{removed} is not removed"
            );
        }
        assert_eq!(
            envs.get(std::ffi::OsStr::new("HERDR_SOCKET_PATH")),
            Some(&Some(std::ffi::OsStr::new("/run/test/herdr.sock")))
        );
        assert_eq!(
            envs.get(std::ffi::OsStr::new("PATH")),
            None,
            "PATH is left alone"
        );
    }

    #[test]
    fn herdr_names_the_socket_it_is_given() {
        let cmd = herdr(Path::new("/run/test/herdr.sock"));
        let socket = cmd
            .get_envs()
            .find(|(k, _)| *k == "HERDR_SOCKET_PATH")
            .and_then(|(_, v)| v);
        assert_eq!(socket, Some(std::ffi::OsStr::new("/run/test/herdr.sock")));
    }

    #[test]
    fn parses_foreground_processes() {
        let f = parse_foreground(PROCESS_INFO).unwrap();
        // herdr 0.9.5 leaves `tty` out of every answer: its handler sets it to
        // nothing (2026-11-19). The field is read for a herdr that fills it in.
        assert_eq!(f.tty, None);
        assert_eq!(f.processes.len(), 3);
        assert_eq!(
            f.processes[2],
            ForegroundProcess {
                name: "some-mcp".to_string(),
                argv: vec![
                    "/home/user/.local/bin/some-mcp".to_string(),
                    "-t".to_string(),
                    "stdio".to_string()
                ],
                pid: 3_719_166,
            }
        );
        assert_eq!(f.processes[1].pid, 3_719_005, "claude's pid");
    }

    #[test]
    fn reads_a_tty_when_herdr_gives_one() {
        let json = r#"{"result":{"process_info":{"pane_id":"w1:p1","tty":"/dev/pts/7",
            "foreground_processes":[{"name":"tmux","argv":["tmux"],"pid":1}]}}}"#;
        let f = parse_foreground(json).unwrap();
        assert_eq!(f.tty.as_deref(), Some("/dev/pts/7"));
        assert_eq!(
            f.processes,
            vec![ForegroundProcess {
                name: "tmux".to_string(),
                argv: vec!["tmux".to_string()],
                pid: 1,
            }]
        );
    }

    fn process(name: &str, argv: &[&str]) -> ForegroundProcess {
        ForegroundProcess {
            name: name.to_string(),
            argv: argv.iter().map(|a| (*a).to_string()).collect(),
            pid: 1,
        }
    }

    /// A tmux client in a herdr pane, as herdr 0.9.5 described one on Linux
    /// on 2026-11-19: the name is the one tmux gives its client, not `tmux`.
    #[test]
    fn a_tmux_client_is_known_by_the_name_tmux_gives_it() {
        let client = process(
            "tmux: client",
            &["tmux", "-f", "/dev/null", "new-session", "-s", "probe"],
        );
        assert!(client.is_tmux());
        assert!(process("tmux", &["tmux"]).is_tmux());
        // By what was run, whatever name it now goes by.
        assert!(process("something", &["/usr/bin/tmux", "attach"]).is_tmux());
        assert!(!process("claude", &["/usr/bin/claude"]).is_tmux());
        assert!(!process("tmuxinator", &["tmuxinator", "start"]).is_tmux());
        assert!(!process("zombie", &[]).is_tmux());
    }

    #[test]
    fn a_process_without_argv_still_parses() {
        let json = r#"{"result":{"process_info":{"pane_id":"w1:p1",
            "foreground_processes":[{"name":"zombie","argv":null,"pid":7},{"name":"sh","pid":8}]}}}"#;
        let f = parse_foreground(json).unwrap();
        assert_eq!(f.processes[0].argv, Vec::<String>::new());
        assert_eq!(f.processes[1].pid, 8);
    }

    #[test]
    fn parses_sessions() {
        let s = parse_sessions(SESSIONS).unwrap();
        assert_eq!(s.iter().filter(|s| s.default).count(), 1);
    }

    #[test]
    fn rejects_an_error_response() {
        assert!(parse_pane_list(r#"{"error":{"code":"x","message":"y"}}"#).is_err());
    }

    /// A command still running at the deadline is given up on near it, and
    /// killed and reaped, not left behind.
    #[test]
    fn a_command_that_does_not_finish_in_time_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let started = Instant::now();
        let out = output_within(
            Command::new("sh")
                .args(["-c", r#"echo $$ > "$1"; exec sleep 5"#, "sh"])
                .arg(&pid_file),
            Duration::from_millis(300),
        )
        .unwrap();
        let took = started.elapsed();
        assert!(out.is_none(), "a command past its deadline gave {out:?}");
        assert!(
            took >= Duration::from_millis(300) && took < Duration::from_secs(2),
            "gave up after {took:?}, not near the 300 ms deadline"
        );
        let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only asks whether the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "the command ({pid}) is still running, or unreaped");
    }

    /// One that finishes in time gives what it printed and how it exited.
    #[test]
    fn a_command_that_finishes_in_time_gives_its_output() {
        let out = output_within(
            Command::new("sh").args(["-c", "printf out; printf err >&2; exit 3"]),
            Duration::from_secs(5),
        )
        .unwrap()
        .expect("a quick command finishes in time");
        assert_eq!(out.stdout, b"out");
        assert_eq!(out.stderr, b"err");
        assert_eq!(out.status.code(), Some(3));
    }

    /// More output than a pipe holds does not leave the command waiting for
    /// a reader, and none of it is lost.
    #[test]
    fn a_command_with_more_output_than_a_pipe_holds_finishes() {
        let out = output_within(
            Command::new("head").args(["-c", "1000000", "/dev/zero"]),
            Duration::from_secs(5),
        )
        .unwrap()
        .expect("a command with a lot to say still finishes");
        assert_eq!(out.stdout.len(), 1_000_000);
        assert!(out.status.success());
    }

    #[test]
    fn herdr_names_each_key() {
        assert_eq!(herdr_key_name(Key::Escape), "esc");
        assert_eq!(herdr_key_name(Key::Backspace), "backspace");
        assert_eq!(herdr_key_name(Key::Char('A')), "A");
    }
}
