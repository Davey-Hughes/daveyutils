//! The injection target abstraction: anything nudge can read a screen from
//! (for banner detection / `--verify`) and send a submitted line of text to.

use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

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
pub const KEYPRESS_PAUSE: Duration = Duration::from_millis(500);

/// A key pressed on its own, as [`Target::send_key`] presses it. Each backend
/// has its own name for it: tmux calls Backspace `BSpace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A key that types a character, pressed for what it does in a Vim mode:
    /// `A`, `i`.
    Char(char),
    Escape,
    Backspace,
}

/// Why the pane a job names is no longer the one it was scheduled for, so
/// that nothing may be typed there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrongPane {
    /// The tmux server that issued the pane's id has been replaced, and every
    /// new server hands out ids from `%0` again, so the id now names whichever
    /// new pane took it, or none.
    ServerRestarted,
    /// The pane holds a different conversation than the one the job was
    /// scheduled for.
    OtherConversation,
    /// A tmux pane that now runs herdr's client, which passes what it is sent
    /// on to whichever herdr pane has focus.
    RunsHerdr,
    /// A herdr pane that now runs tmux's client, which passes what it is sent
    /// on to whichever tmux pane is active.
    RunsTmux,
}

/// A target's screen geometry at the moment of a capture.
///
/// Persisted alongside a `--verify` job's fingerprint because two captures are
/// only comparable when they were taken at the same size: a resize reflows the
/// whole pane, so every line can change without the user touching anything.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneDims {
    pub width: u16,
    pub height: u16,
}

impl PaneDims {
    /// Parse tmux's `#{pane_width}x#{pane_height}`.
    ///
    /// Strict on purpose, and `None` rather than a default on anything it does
    /// not fully understand. `tmux display-message -p -t <gone> '#{pane_width}x
    /// #{pane_height}'` prints a bare `"x"` and **exits 0** — a missing pane is
    /// not an error, it is empty fields. Defaulting those to `0x0` would make a
    /// dead pane's dims compare *equal* to another dead pane's, promoting
    /// "we have no idea how big this is" into "comparable", which is how a
    /// false SKIP gets made. Unparseable means unknown means fail open.
    pub fn parse(s: &str) -> Option<PaneDims> {
        let (w, h) = s.trim().split_once('x')?;
        Some(PaneDims {
            width: w.trim().parse().ok()?,
            height: h.trim().parse().ok()?,
        })
    }
}

/// A place nudge can read from and type into. `job::TargetSpec` is the
/// serializable *descriptor* of one of these; this trait is the runtime
/// *behavior*.
pub trait Target {
    /// Capture the target's current visible screen text.
    fn capture(&self) -> Result<String>;

    /// The same capture with its SGR styling left in, for telling what is
    /// typed in an input box from what is drawn dim there.
    fn capture_styled(&self) -> Result<String>;

    /// Type `text` into the target and submit it (as if Enter were pressed),
    /// returning once the target has had time to act on the Enter.
    fn send_line(&self, text: &str) -> Result<()>;

    /// Press `key` by itself, as a person would: a keypress the target reads on
    /// its own, not the first character of whatever is typed next.
    fn send_key(&self, key: Key) -> Result<()>;

    /// Why the target is no longer the pane the job was scheduled for, or
    /// `None` if it still is. Asked before anything is read or typed, since a
    /// wrong pane gets nothing. `Err` when this cannot be checked, which says
    /// the target is unreachable, not that it moved.
    fn wrong_pane(&self) -> Result<Option<WrongPane>>;

    /// Wait a moment, for a pane slow to redraw to catch up before it is read
    /// again.
    fn settle(&self);

    /// The target's current geometry, or `None` if it cannot be determined.
    ///
    /// `None`, not `Err`: not knowing the size is never a reason to fail a
    /// schedule or a fire. Every caller treats it as "not comparable" and falls
    /// back to the banner check, which is what nudge did before `--verify` had
    /// any notion of recency.
    fn dims(&self) -> Option<PaneDims>;
}

#[cfg(test)]
pub(crate) mod fake;
pub mod herdr;
pub mod tmux;

#[cfg(test)]
mod tests {
    use super::PaneDims;

    #[test]
    fn parses_tmux_dims_and_rejects_everything_else() {
        assert_eq!(
            PaneDims::parse("80x24\n"),
            Some(PaneDims {
                width: 80,
                height: 24
            })
        );
        // The one that matters: tmux prints this, and exits 0, when the pane is
        // gone. Read as 0x0 it would compare equal to the next dead pane and
        // manufacture a confident "unchanged"/"changed" verdict out of nothing.
        assert_eq!(PaneDims::parse("x"), None, "empty fields are not 0x0");
        for junk in ["", "80", "80x", "x24", "eightyx24", "80x24x30", "-1x24"] {
            assert_eq!(PaneDims::parse(junk), None, "{junk:?} must not parse");
        }
    }
}
