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
            let _ = std::fs::remove_file(&unit);
            println!("nudge: removed {}", unit.display());
        }
        Manager::Launchd => {
            let uid = current_uid();
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &format!("gui/{uid}/{}", launchd::LABEL)])
                .status();
            let plist = launchd::plist_path(&home);
            let _ = std::fs::remove_file(&plist);
            println!("nudge: removed {}", plist.display());
        }
    }
    Ok(())
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
