//! launchd LaunchAgent generation.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::InstallPlan;

pub const LABEL: &str = "com.nudge.daemon";

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct KeepAlive {
    successful_exit: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LaunchAgent {
    label: String,
    program_arguments: Vec<String>,
    run_at_load: bool,
    keep_alive: KeepAlive,
}

/// XML plist that runs `<exec> --daemon` at load and restarts it only on an
/// unsuccessful exit. A bare `KeepAlive=true` restarts on ANY exit — including
/// the clean exit `run` now performs when it loses the singleton lock race
/// (see `lib::run`'s `WouldBlock` handling), which would otherwise loop forever.
pub fn plist_bytes(exec: &Path) -> Vec<u8> {
    let agent = LaunchAgent {
        label: LABEL.to_string(),
        program_arguments: vec![exec.display().to_string(), "--daemon".to_string()],
        run_at_load: true,
        keep_alive: KeepAlive {
            successful_exit: false,
        },
    };
    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, &agent).expect("serialize launchd plist");
    buf
}

/// `<home>/Library/LaunchAgents/com.nudge.daemon.plist`.
pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// Files to write and commands to run to install the LaunchAgent.
pub fn install_plan(exec: &Path, plist_path: &Path, uid: u32) -> InstallPlan {
    let xml = String::from_utf8(plist_bytes(exec)).expect("plist is valid utf-8");
    InstallPlan {
        files: vec![(plist_path.to_path_buf(), xml)],
        commands: vec![vec![
            "launchctl".into(),
            "bootstrap".into(),
            format!("gui/{uid}"),
            plist_path.display().to_string(),
        ]],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn plist_has_label_program_and_flags() {
        let bytes = plist_bytes(Path::new("/usr/local/bin/nudge"));
        let xml = String::from_utf8(bytes).unwrap();
        assert!(xml.contains("com.nudge.daemon"), "got:\n{xml}");
        assert!(xml.contains("/usr/local/bin/nudge"));
        assert!(xml.contains("--daemon"));
        assert!(xml.contains("RunAtLoad"));
        // KeepAlive must restart only on an unsuccessful exit, not on ANY exit --
        // a bare `KeepAlive=true` would restart even a clean exit (e.g. when
        // `run` loses the singleton lock race), looping forever.
        assert!(xml.contains("KeepAlive"), "got:\n{xml}");
        let after_key = xml
            .split("<key>SuccessfulExit</key>")
            .nth(1)
            .expect("SuccessfulExit key present");
        assert!(
            after_key.trim_start().starts_with("<false/>"),
            "SuccessfulExit must be false, got:\n{xml}"
        );
    }

    #[test]
    fn plist_path_is_under_launch_agents() {
        let p = plist_path(Path::new("/Users/d"));
        assert_eq!(
            p,
            Path::new("/Users/d/Library/LaunchAgents/com.nudge.daemon.plist")
        );
    }

    #[test]
    fn install_plan_bootstraps_the_agent() {
        let p = Path::new("/Users/d/Library/LaunchAgents/com.nudge.daemon.plist");
        let plan = install_plan(Path::new("/usr/local/bin/nudge"), p, 501);
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].0, p);
        assert!(plan.commands.contains(&vec![
            "launchctl".into(),
            "bootstrap".into(),
            "gui/501".into(),
            "/Users/d/Library/LaunchAgents/com.nudge.daemon.plist".into()
        ]));
    }
}
