//! Register (and unregister) the nudge daemon with the OS user service
//! manager. Generation is pure and tested; `install`/`uninstall` actually
//! touch the host and are never called by tests.

pub mod launchd;
pub mod systemd;

use std::path::PathBuf;

/// The concrete steps to register the daemon: files to write, then commands to
/// run.
#[derive(Debug, PartialEq)]
pub struct InstallPlan {
    pub files: Vec<(PathBuf, String)>,
    pub commands: Vec<Vec<String>>,
}
