//! Register (and unregister) the nudge daemon with the OS user service
//! manager. Generation is pure and tested; `install`/`uninstall` actually
//! touch the host and are never called by tests.

pub mod launchd;
pub mod systemd;

use std::path::{Path, PathBuf};

/// The concrete steps to register the daemon: files to write, then commands to
/// run.
#[derive(Debug, PartialEq)]
pub struct InstallPlan {
    pub files: Vec<(PathBuf, String)>,
    pub commands: Vec<Vec<String>>,
}

/// Which OS user service manager to target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Systemd,
    Launchd,
}

impl Manager {
    /// The manager for the current OS: `launchd` on macOS, `systemd` --user
    /// everywhere else.
    pub fn current() -> Manager {
        if cfg!(target_os = "macos") {
            Manager::Launchd
        } else {
            Manager::Systemd
        }
    }
}

/// Pure dispatch: the install plan for a given manager.
pub fn plan_for(
    manager: Manager,
    exec: &Path,
    home: &Path,
    xdg_config: Option<&Path>,
    uid: u32,
) -> InstallPlan {
    match manager {
        Manager::Systemd => {
            let unit = systemd::unit_path(home, xdg_config);
            systemd::install_plan(exec, &unit)
        }
        Manager::Launchd => {
            let plist = launchd::plist_path(home);
            launchd::install_plan(exec, &plist, uid)
        }
    }
}

/// Write the plan's files and run its commands. EFFECTFUL — touches the host
/// service manager. Only ever called from an explicit CLI opt-in, never tests.
pub fn install(exec: &Path) -> anyhow::Result<()> {
    // An ad-hoc daemon (auto-started by `nudge -p ...`) still owns the socket.
    // Enabling the unit now would start a second daemon that immediately dies on
    // the singleton lock, and systemd would retry it every RestartSec forever.
    // But if the live daemon IS our managed unit, re-running --install-daemon
    // (e.g. after the binary moved) is idempotent and must not be blocked.
    let paths = crate::paths::resolve();
    if std::os::unix::net::UnixStream::connect(&paths.socket).is_ok() && !managed_daemon_is_active()
    {
        anyhow::bail!(
            "a nudge daemon is already running (socket {}).\n\
             Stop it first, then re-run --install-daemon:\n  pkill -f 'nudge --daemon'",
            paths.socket.display()
        );
    }

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set"))?;
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let uid = current_uid();
    let plan = plan_for(Manager::current(), exec, &home, xdg_config.as_deref(), uid);

    for (path, contents) in &plan.files {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, contents)?;
        println!("nudge: wrote {}", path.display());
    }
    for cmd in &plan.commands {
        let (prog, args) = cmd.split_first().expect("non-empty command");
        let status = std::process::Command::new(prog).args(args).status()?;
        if !status.success() {
            anyhow::bail!("`{}` failed with {status}", cmd.join(" "));
        }
    }
    if Manager::current() == Manager::Systemd {
        println!(
            "nudge: if this is a headless / SSH session, run once:\n  \
             loginctl enable-linger $USER"
        );
    }
    println!("nudge: daemon registered.");
    Ok(())
}

/// What to report about the attempt to remove the registration file at `path`.
///
/// Pure so the claim is testable: `uninstall` itself reads $HOME and shells out
/// to systemctl/launchctl, so it is never called by tests -- which is exactly
/// how it shipped announcing a removal it had not performed.
fn removal_report(path: &Path, result: std::io::Result<()>) -> String {
    match result {
        Ok(()) => format!("nudge: removed {}", path.display()),
        // Not an error: --install-daemon is optional (the daemon auto-starts
        // via ensure_daemon), so having nothing to remove is the common case.
        // It is just not a removal, and must not be reported as one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            format!("nudge: no registration found at {}", path.display())
        }
        // The registration is still on disk and still live. Distinct from
        // NotFound, which needs no action.
        Err(e) => format!("nudge: could not remove {}: {e}", path.display()),
    }
}

/// Disable and remove the daemon registration. EFFECTFUL; never called by tests.
pub fn uninstall() -> anyhow::Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set"))?;
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    match Manager::current() {
        Manager::Systemd => {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "disable", "--now", "nudged.service"])
                .status();
            let unit = systemd::unit_path(&home, xdg_config.as_deref());
            println!("{}", removal_report(&unit, std::fs::remove_file(&unit)));
        }
        Manager::Launchd => {
            let uid = current_uid();
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &format!("gui/{uid}/{}", launchd::LABEL)])
                .status();
            let plist = launchd::plist_path(&home);
            println!("{}", removal_report(&plist, std::fs::remove_file(&plist)));
        }
    }

    // Removing the registration only stops a *managed* daemon. An ad-hoc one
    // (auto-started by `nudge -p ...`) still holds the socket and will still
    // fire every pending job -- so leaving the command's output at "removed
    // <unit>" tells the user the daemon is gone while it demonstrably is not.
    // `install` pings the same socket for the same reason.
    let paths = crate::paths::resolve();
    if std::os::unix::net::UnixStream::connect(&paths.socket).is_ok() {
        println!(
            "nudge: note: a daemon is still running (socket {}) and will still fire \
             pending jobs.\n  It was not started by the registration just removed. \
             Stop it with:\n  pkill -f 'nudge --daemon'",
            paths.socket.display()
        );
    }
    Ok(())
}

/// Is the daemon currently answering actually our managed service (as opposed to
/// an ad-hoc one auto-started by `nudge -p ...`)? Re-installing over our own unit
/// is idempotent and fine; starting a unit over an ad-hoc daemon is not.
fn managed_daemon_is_active() -> bool {
    match Manager::current() {
        Manager::Systemd => std::process::Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", "nudged.service"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        Manager::Launchd => std::process::Command::new("launchctl")
            .args([
                "print",
                &format!("gui/{}/{}", current_uid(), launchd::LABEL),
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false),
    }
}

/// Current user's uid (via `id -u`, portable across Linux/macOS without libc).
fn current_uid() -> u32 {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn uninstall_does_not_claim_a_removal_that_never_happened() {
        // The common case: the daemon auto-starts via ensure_daemon, so a user
        // who never ran --install-daemon has no unit file at all. `let _ =
        // remove_file(..)` then discarded the ENOENT and printed "removed
        // <path>" for a file that never existed, exit 0.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nudged.service");

        let report = removal_report(&missing, std::fs::remove_file(&missing));

        assert!(
            !report.contains("removed"),
            "nothing was removed, so the report must not say so: {report}"
        );
        assert!(
            report.contains(&missing.display().to_string()),
            "the report must still name the path it looked at: {report}"
        );
    }

    #[test]
    fn uninstall_reports_a_real_removal() {
        // The other half: this must stay affirmative on success, or "don't
        // claim a removal" could be satisfied by never claiming one.
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("nudged.service");
        std::fs::write(&unit, b"[Unit]").unwrap();

        let report = removal_report(&unit, std::fs::remove_file(&unit));

        assert!(
            report.contains("removed"),
            "a real removal must be reported as one: {report}"
        );
        assert!(!unit.exists(), "and the file must actually be gone");
    }

    #[test]
    fn uninstall_reports_a_removal_that_failed_for_a_real_reason() {
        // ENOENT means "there was nothing to do"; anything else means the
        // registration is still on disk and still live. Collapsing the two
        // would report a permission failure as a clean no-op.
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("nudged.service");
        std::fs::create_dir(&unit).unwrap(); // remove_file on a dir: not NotFound

        let report = removal_report(&unit, std::fs::remove_file(&unit));

        assert!(
            !report.contains("nudge: removed"),
            "the unit is still there, so this is not a removal: {report}"
        );
        assert!(
            report.contains("could not"),
            "a real failure must say so rather than read as a clean no-op: {report}"
        );
    }

    #[test]
    fn plan_for_systemd_writes_a_unit_and_enables() {
        let plan = plan_for(
            Manager::Systemd,
            Path::new("/usr/bin/nudge"),
            Path::new("/home/d"),
            None,
            1000,
        );
        assert!(plan.files[0].0.ends_with("systemd/user/nudged.service"));
        assert!(plan
            .commands
            .iter()
            .any(|c| c.contains(&"enable".to_string())));
    }

    #[test]
    fn plan_for_launchd_writes_a_plist_and_bootstraps() {
        let plan = plan_for(
            Manager::Launchd,
            Path::new("/usr/local/bin/nudge"),
            Path::new("/Users/d"),
            None,
            501,
        );
        assert!(plan.files[0]
            .0
            .ends_with("Library/LaunchAgents/com.nudge.daemon.plist"));
        assert!(plan
            .commands
            .iter()
            .any(|c| c.contains(&"bootstrap".to_string())));
    }
}
