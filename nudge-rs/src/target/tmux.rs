//! A tmux pane as an injection `Target`, via `tmux capture-pane` / `send-keys`.

use anyhow::{bail, Context, Result};
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

/// A specific tmux pane, addressed by tmux's target syntax (e.g. "bot:0.1").
/// An optional server socket (`tmux -L <socket>`) supports non-default servers
/// and test isolation.
pub struct TmuxTarget {
    pane: String,
    socket: Option<String>,
}

impl TmuxTarget {
    pub fn new(pane: impl Into<String>) -> Self {
        Self {
            pane: pane.into(),
            socket: None,
        }
    }

    pub fn with_socket(pane: impl Into<String>, socket: impl Into<String>) -> Self {
        Self {
            pane: pane.into(),
            socket: Some(socket.into()),
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

impl Target for TmuxTarget {
    fn capture(&self) -> Result<String> {
        let out = self.run(&["capture-pane", "-p", "-t", &self.pane])?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn send_line(&self, text: &str) -> Result<()> {
        // `-l` sends the text literally so tmux doesn't interpret key names;
        // a separate `Enter`, after KEYPRESS_PAUSE, submits it.
        self.run(&["send-keys", "-t", &self.pane, "-l", "--", text])?;
        std::thread::sleep(KEYPRESS_PAUSE);
        self.run(&["send-keys", "-t", &self.pane, "Enter"])?;
        Ok(())
    }

    fn send_key(&self, key: &str) -> Result<()> {
        // The pause comes after the key, so that the caller's next send cannot
        // run into it.
        self.run(&["send-keys", "-t", &self.pane, "-l", "--", key])?;
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
