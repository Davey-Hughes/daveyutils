//! Command implementations for the CLI modes.

use crate::cli::Cli;

/// Dispatch non-daemon modes. (Scheduling / list / cancel / edit added in later tasks.)
pub fn dispatch(_cli: Cli) -> anyhow::Result<()> {
    anyhow::bail!("scheduling not implemented yet");
}
