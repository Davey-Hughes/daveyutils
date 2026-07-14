//! systemd `--user` service generation.

use std::path::{Path, PathBuf};

use super::InstallPlan;
use crate::paths::config_dir;

/// The `.service` unit text that runs `<exec> --daemon`.
pub fn unit_text(exec: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=nudge — rate-limit auto-resumer daemon\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={} --daemon\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec.display()
    )
}

/// `<config_dir>/systemd/user/nudged.service`.
pub fn unit_path(home: &Path, xdg_config: Option<&Path>) -> PathBuf {
    config_dir(home, xdg_config).join("systemd/user/nudged.service")
}

/// Files to write and commands to run to install the systemd unit.
pub fn install_plan(exec: &Path, unit_path: &Path) -> InstallPlan {
    InstallPlan {
        files: vec![(unit_path.to_path_buf(), unit_text(exec))],
        commands: vec![
            vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
            vec![
                "systemctl".into(),
                "--user".into(),
                "enable".into(),
                "--now".into(),
                "nudged.service".into(),
            ],
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn unit_text_runs_the_daemon_and_is_installable() {
        let t = unit_text(Path::new("/usr/bin/nudge"));
        assert!(t.contains("ExecStart=/usr/bin/nudge --daemon"), "got:\n{t}");
        assert!(t.contains("[Install]"));
        assert!(t.contains("WantedBy=default.target"));
        assert!(t.contains("Restart=on-failure"));
    }

    #[test]
    fn unit_path_is_under_systemd_user() {
        let p = unit_path(Path::new("/home/d"), None);
        assert_eq!(p, Path::new("/home/d/.config/systemd/user/nudged.service"));
        let p2 = unit_path(Path::new("/home/d"), Some(Path::new("/cfg")));
        assert_eq!(p2, Path::new("/cfg/systemd/user/nudged.service"));
    }

    #[test]
    fn install_plan_writes_unit_and_enables() {
        let plan = install_plan(
            Path::new("/usr/bin/nudge"),
            Path::new("/home/d/.config/systemd/user/nudged.service"),
        );
        assert_eq!(plan.files.len(), 1);
        assert_eq!(
            plan.files[0].0,
            Path::new("/home/d/.config/systemd/user/nudged.service")
        );
        assert!(plan.files[0]
            .1
            .contains("ExecStart=/usr/bin/nudge --daemon"));
        // daemon-reload then enable --now.
        assert!(plan.commands.contains(&vec![
            "systemctl".into(),
            "--user".into(),
            "daemon-reload".into()
        ]));
        assert!(plan.commands.contains(&vec![
            "systemctl".into(),
            "--user".into(),
            "enable".into(),
            "--now".into(),
            "nudged.service".into()
        ]));
    }
}
