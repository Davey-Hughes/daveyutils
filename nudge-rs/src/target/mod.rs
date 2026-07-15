//! The injection target abstraction: anything nudge can read a screen from
//! (for banner detection / `--verify`) and send a submitted line of text to.

use anyhow::Result;

/// A place nudge can read from and type into. `job::Target` is the serializable
/// *descriptor* of one of these; this trait is the runtime *behavior*.
pub trait Target {
    /// Capture the target's current visible screen text.
    fn capture(&self) -> Result<String>;

    /// Type `text` into the target and submit it (as if Enter were pressed).
    fn send_line(&self, text: &str) -> Result<()>;
}

pub mod tmux;
