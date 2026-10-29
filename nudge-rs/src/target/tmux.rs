//! A tmux pane as an injection `Target`, via `tmux capture-pane` / `send-keys`.

use anyhow::{Context, Result, bail};
use std::process::{Command, Output};
use std::time::Duration;

use super::{PaneDims, Target};

/// How long to leave between typed text and a key that has to be read as a
/// keypress of its own: the Enter after a message, or a key pressed before one.
///
/// Codex reads keystrokes that arrive faster than anyone types as a paste, and
/// for 120ms after the last of them (`PASTE_ENTER_SUPPRESS_WINDOW` in codex-rs
/// `tui/src/bottom_pane/paste_burst.rs`) takes Enter as a newline inside that
/// paste rather than a submit. `send-keys -l` delivers the whole message at
/// once and the Enter followed it by about 2ms, so in Codex the nudge sat in
/// the composer, unsent. Several times that window, and still nothing a person
/// waiting on a rate limit would notice.
const KEYPRESS_PAUSE: Duration = Duration::from_millis(500);

/// What a tmux server is asked for to tell it from any other, as
/// `#{start_time}-#{pid}`: `1793226261-4820`.
///
/// A pane id (`%19`) is unique only for the life of the server that issued
/// it. A new server numbers its panes from `%0` again, so after a reboot or a
/// `kill-server` a stored id names whichever new pane took it. The start time
/// is what tells two servers apart across a reboot, where the PID can come
/// round again: tmux starts early in a login. The PID tells apart two servers
/// started within the same second.
pub const SERVER_FORMAT: &str = "#{start_time}-#{pid}";

/// [`SERVER_FORMAT`]'s answer, or `None` if it is not one: a tmux too old to
/// know `start_time` answers `-4820`.
pub fn parse_server(s: &str) -> Option<String> {
    let s = s.trim();
    let (started, pid) = s.split_once('-')?;
    let number = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    (number(started) && number(pid)).then(|| s.to_string())
}

/// What [`TmuxTarget::locate`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneLocation {
    /// tmux's id for the pane, e.g. `%19`.
    pub id: String,
    /// Where it sits now, as `session:window.pane`.
    pub position: String,
    /// The server the id belongs to, as [`SERVER_FORMAT`] reads it, or `None`
    /// if tmux would not say.
    pub server: Option<String>,
}

/// A specific tmux pane, addressed by tmux's target syntax (e.g. "bot:0.1").
/// An optional server socket (`tmux -L <socket>`) supports non-default servers
/// and test isolation.
pub struct TmuxTarget {
    pane: String,
    socket: Option<String>,
    /// The server the pane's id was read from, if the job kept it.
    server: Option<String>,
}

impl TmuxTarget {
    pub fn new(pane: impl Into<String>) -> Self {
        Self {
            pane: pane.into(),
            socket: None,
            server: None,
        }
    }

    /// Expect the pane to be on `server` (see [`SERVER_FORMAT`]), so that
    /// [`Target::server_restarted`] asks tmux. With `None` there is nothing to
    /// compare, as for a job queued before jobs kept their server.
    #[must_use]
    pub fn expecting_server(mut self, server: Option<String>) -> Self {
        self.server = server;
        self
    }

    pub fn with_socket(pane: impl Into<String>, socket: impl Into<String>) -> Self {
        Self {
            pane: pane.into(),
            socket: Some(socket.into()),
            server: None,
        }
    }

    /// tmux's id for this pane, and where the pane sits right now.
    ///
    /// The id (`%19`) is what a job stores. A position (`main:12.0`) is an
    /// index, and closing a window renumbers every window after it (with
    /// `renumber-windows` on), so a stored position goes on to name whichever
    /// pane moves into the slot. The id stays with the pane until it closes.
    ///
    /// `display-message -t` cannot be asked first. When the window it names
    /// is gone it answers for the current pane and exits 0, so a typo'd
    /// window would be stored as the id of whatever pane you happen to be in.
    /// `capture-pane` fails on the same target, so it checks the pane exists.
    pub fn locate(&self) -> Result<PaneLocation> {
        self.run(&["capture-pane", "-p", "-t", &self.pane, "-S", "0", "-E", "0"])
            .with_context(|| format!("no tmux pane {}", self.pane))?;
        let out = self.run(&[
            "display-message",
            "-p",
            "-t",
            &self.pane,
            // The server first, from the same answer as the id it vouches for.
            &format!(
                "{SERVER_FORMAT}\t#{{pane_id}}\t#{{session_name}}:#{{window_index}}.#{{pane_index}}"
            ),
        ])?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut fields = text.trim_end_matches('\n').splitn(3, '\t');
        match (fields.next(), fields.next(), fields.next()) {
            (Some(server), Some(id), Some(position)) if id.starts_with('%') => Ok(PaneLocation {
                id: id.to_string(),
                position: position.to_string(),
                server: parse_server(server),
            }),
            _ => bail!("no tmux pane {}", self.pane),
        }
    }

    /// Capture the pane WITH SGR escape sequences (`-e`), for a colored preview.
    /// The plain `capture()` (the `Target` method used by `--verify`'s
    /// fingerprint) stays escape-free, so the recency gate is unaffected.
    pub fn capture_escaped(&self) -> Result<String> {
        let out = self.run(&["capture-pane", "-e", "-p", "-t", &self.pane])?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run a tmux subcommand (with the configured socket, if any), erroring on
    /// non-zero exit.
    fn run(&self, args: &[&str]) -> Result<Output> {
        let mut cmd = Command::new("tmux");
        if let Some(sock) = &self.socket {
            cmd.args(["-L", sock]);
        }
        cmd.args(args);
        let out = cmd.output().context("failed to run tmux")?;
        if !out.status.success() {
            bail!(
                "tmux {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out)
    }
}

/// Whether tmux's complaint says there is no server to talk to, as opposed to
/// a server that would not answer.
fn no_server_running(complaint: &str) -> bool {
    complaint.contains("no server running")
        || (complaint.contains("error connecting to")
            && complaint.contains("No such file or directory"))
}

impl Target for TmuxTarget {
    fn capture(&self) -> Result<String> {
        let out = self.run(&["capture-pane", "-p", "-t", &self.pane])?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn capture_styled(&self) -> Result<String> {
        self.capture_escaped()
    }

    fn send_line(&self, text: &str) -> Result<()> {
        // `-l` sends the text literally so tmux doesn't interpret key names;
        // a separate `Enter`, after KEYPRESS_PAUSE, submits it.
        self.run(&["send-keys", "-t", &self.pane, "-l", "--", text])?;
        std::thread::sleep(KEYPRESS_PAUSE);
        self.run(&["send-keys", "-t", &self.pane, "Enter"])?;
        // And after it, so a capture taken next shows what the Enter did.
        std::thread::sleep(KEYPRESS_PAUSE);
        Ok(())
    }

    fn server_restarted(&self) -> Result<bool> {
        let Some(expected) = &self.server else {
            return Ok(false);
        };
        // No `-t`: the start time and PID belong to the server and read the
        // same from any pane, and the pane the job names may not exist here.
        let out = match self.run(&["display-message", "-p", SERVER_FORMAT]) {
            Ok(out) => out,
            // The server that issued the id cannot be gone and still be
            // running, so for a stamped target "no server at all" proves the
            // stamped one is gone. tmux says so two ways: a killed server
            // leaves its socket behind (`no server running on
            // /tmp/tmux-1000/default`), and a reboot removes it (`error
            // connecting to /tmp/tmux-1000/default (No such file or
            // directory)`).
            Err(e) if no_server_running(&format!("{e:#}")) => return Ok(true),
            Err(e) => return Err(e),
        };
        let now = parse_server(&String::from_utf8_lossy(&out.stdout))
            .context("tmux would not say when its server started")?;
        Ok(&now != expected)
    }

    fn settle(&self) {
        std::thread::sleep(KEYPRESS_PAUSE);
    }

    fn send_key(&self, key: &str) -> Result<()> {
        // A key name, so no `-l`: `BSpace` is Backspace, not seven letters.
        // The pause comes after the key, so that the caller's next send cannot
        // run into it.
        self.run(&["send-keys", "-t", &self.pane, "--", key])?;
        std::thread::sleep(KEYPRESS_PAUSE);
        Ok(())
    }

    /// Ask tmux for the pane's size rather than inferring it from the capture.
    ///
    /// The capture cannot answer this. Height is recoverable from the line
    /// count, but width is invisible in the text — nothing in a capture
    /// distinguishes an 80-column pane from a 200-column one holding the same
    /// short lines. A width-only resize still reflows any wrapped line, so
    /// deriving dims from the capture would miss exactly the resize that
    /// changes the fingerprint, and skip. tmux knows; ask it.
    fn dims(&self) -> Option<PaneDims> {
        let out = self
            .run(&[
                "display-message",
                "-p",
                "-t",
                &self.pane,
                "#{pane_width}x#{pane_height}",
            ])
            .ok()?;
        PaneDims::parse(&String::from_utf8_lossy(&out.stdout))
    }
}

#[cfg(test)]
mod tests {
    use super::{no_server_running, parse_server};

    #[test]
    fn parses_a_server_start_time_and_pid() {
        assert_eq!(
            parse_server("1793226261-4820\n"),
            Some("1793226261-4820".to_string())
        );
    }

    /// A tmux without `start_time` leaves that half empty. An empty stamp
    /// would match the next empty one, so it is no stamp at all.
    #[test]
    fn refuses_anything_but_two_numbers() {
        for s in ["-4820", "1793226261-", "", "x-1", "1-2-3", "1793226261"] {
            assert_eq!(parse_server(s), None, "{s:?}");
        }
    }

    #[test]
    fn both_ways_tmux_says_there_is_no_server() {
        assert!(no_server_running(
            "tmux [\"display-message\"] failed: no server running on /tmp/tmux-1000/default"
        ));
        assert!(no_server_running(
            "tmux [\"display-message\"] failed: error connecting to /tmp/tmux-1000/default (No such file or directory)"
        ));
    }

    #[test]
    fn other_complaints_are_not_a_missing_server() {
        assert!(!no_server_running(
            "tmux failed: error connecting to /tmp/tmux-1000/default (Permission denied)"
        ));
        assert!(!no_server_running("tmux failed: protocol version mismatch"));
        assert!(!no_server_running("failed to run tmux"));
    }
}
