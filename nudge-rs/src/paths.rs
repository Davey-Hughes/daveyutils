//! Cross-platform locations for nudge's state file and IPC socket.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Macos,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub state_dir: PathBuf,
    pub queue: PathBuf,
    pub socket: PathBuf,
}

/// A set-but-empty XDG variable means *unset*, per the XDG basedir spec.
///
/// Filtered here rather than at the `std::env::var_os` call sites because this
/// is the seam tests can reach: `resolve()` reads the process environment and
/// so cannot be pinned, and an empty value that slips through is silent rather
/// than loud -- `PathBuf::from("")` is a *relative* path, so every path derived
/// from it resolves against the caller's working directory instead of failing.
fn non_empty(p: Option<&Path>) -> Option<&Path> {
    p.filter(|p| !p.as_os_str().is_empty())
}

/// Resolve paths from explicit inputs (pure; used by tests and `resolve`).
pub fn resolve_from(
    home: &Path,
    xdg_state: Option<&Path>,
    xdg_runtime: Option<&Path>,
    os: Os,
) -> Paths {
    let state_dir = match os {
        Os::Linux => non_empty(xdg_state)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".local/state"))
            .join("nudge"),
        Os::Macos => home.join("Library/Application Support/nudge"),
    };

    // The socket belongs in a runtime dir when one exists (Linux); otherwise it
    // lives beside the state file.
    let socket_dir = match os {
        Os::Linux => non_empty(xdg_runtime)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| state_dir.clone()),
        Os::Macos => state_dir.clone(),
    };

    Paths {
        queue: state_dir.join("queue.json"),
        socket: socket_dir.join("nudge.sock"),
        state_dir,
    }
}

/// The user config dir: `$XDG_CONFIG_HOME`, else `<home>/.config`.
pub fn config_dir(home: &Path, xdg_config: Option<&Path>) -> PathBuf {
    non_empty(xdg_config)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"))
}

/// Resolve paths from the current environment and OS.
pub fn resolve() -> Paths {
    let home = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h),
        None => {
            eprintln!("nudge: warning: $HOME is unset; using '.' for state paths");
            PathBuf::from(".")
        }
    };
    let xdg_state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    let xdg_runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let os = if cfg!(target_os = "macos") {
        Os::Macos
    } else {
        Os::Linux
    };
    resolve_from(&home, xdg_state.as_deref(), xdg_runtime.as_deref(), os)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn linux_prefers_xdg_state_and_runtime() {
        let p = resolve_from(
            Path::new("/home/d"),
            Some(Path::new("/home/d/.local/state")),
            Some(Path::new("/run/user/1000")),
            Os::Linux,
        );
        assert_eq!(p.state_dir, Path::new("/home/d/.local/state/nudge"));
        assert_eq!(p.queue, Path::new("/home/d/.local/state/nudge/queue.json"));
        assert_eq!(p.socket, Path::new("/run/user/1000/nudge.sock"));
    }

    #[test]
    fn linux_falls_back_to_home_when_xdg_unset() {
        let p = resolve_from(Path::new("/home/d"), None, None, Os::Linux);
        assert_eq!(p.state_dir, Path::new("/home/d/.local/state/nudge"));
        // No XDG_RUNTIME_DIR -> socket sits in the state dir.
        assert_eq!(p.socket, Path::new("/home/d/.local/state/nudge/nudge.sock"));
    }

    #[test]
    fn an_empty_xdg_var_means_unset_not_cwd() {
        // A set-but-empty XDG var is common in cron jobs, systemd units and
        // some login shells. `PathBuf::from("")` is a *relative* path, so
        // joining onto it silently yields CWD-relative state: the CLI and the
        // daemon then resolve the socket against their own working directory,
        // and scheduling from ~/projects/a while listing from ~/projects/b
        // talks to a different socket and a different queue.json. The XDG spec
        // says an empty value must be treated as unset.
        let p = resolve_from(
            Path::new("/home/d"),
            Some(Path::new("")),
            Some(Path::new("")),
            Os::Linux,
        );
        assert!(
            p.queue.is_absolute(),
            "empty XDG_STATE_HOME must not yield a relative queue: {:?}",
            p.queue
        );
        assert!(
            p.socket.is_absolute(),
            "empty XDG_RUNTIME_DIR must not yield a relative socket: {:?}",
            p.socket
        );
        // Empty == unset, so both must land exactly where the unset case does.
        assert_eq!(p, resolve_from(Path::new("/home/d"), None, None, Os::Linux));
    }

    #[test]
    fn an_empty_xdg_config_home_means_unset_not_cwd() {
        let d = config_dir(Path::new("/home/d"), Some(Path::new("")));
        assert_eq!(d, Path::new("/home/d/.config"));
    }

    #[test]
    fn config_dir_prefers_xdg_config_home() {
        let d = config_dir(Path::new("/home/d"), Some(Path::new("/cfg")));
        assert_eq!(d, Path::new("/cfg"));
    }

    #[test]
    fn config_dir_falls_back_to_home_dot_config() {
        let d = config_dir(Path::new("/home/d"), None);
        assert_eq!(d, Path::new("/home/d/.config"));
    }

    #[test]
    fn macos_uses_application_support() {
        let p = resolve_from(Path::new("/Users/d"), None, None, Os::Macos);
        assert_eq!(
            p.state_dir,
            Path::new("/Users/d/Library/Application Support/nudge")
        );
        assert_eq!(
            p.socket,
            Path::new("/Users/d/Library/Application Support/nudge/nudge.sock")
        );
    }
}
