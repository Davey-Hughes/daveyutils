//! The injection target abstraction: anything nudge can read a screen from
//! (for banner detection / `--verify`) and send a submitted line of text to.

use anyhow::Result;
use serde::{Deserialize, Serialize};

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

    /// Type `text` into the target and submit it (as if Enter were pressed).
    fn send_line(&self, text: &str) -> Result<()>;

    /// The target's current geometry, or `None` if it cannot be determined.
    ///
    /// `None`, not `Err`: not knowing the size is never a reason to fail a
    /// schedule or a fire. Every caller treats it as "not comparable" and falls
    /// back to the banner check, which is what nudge did before `--verify` had
    /// any notion of recency.
    fn dims(&self) -> Option<PaneDims>;
}

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
