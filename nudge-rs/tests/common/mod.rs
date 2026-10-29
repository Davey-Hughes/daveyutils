//! Shared fixtures for nudge's CLI integration tests.
//!
//! `#![allow(dead_code)]` because this module is compiled once per consuming
//! test binary (cargo treats every file under `tests/` as its own crate) and
//! most consumers only need one or two of these items -- the rest would
//! otherwise warn as unused in every binary that doesn't happen to call them.
#![allow(dead_code)]

use std::path::Path;

use tempfile::TempDir;

/// The byte budget every socket path built by these tests must fit inside.
///
/// A Unix domain socket path lives in `sockaddr_un.sun_path`, which is capped
/// at 104 bytes on macOS and 108 on Linux. CI only runs Linux, so 108 is the
/// only bound a Linux-only run would ever enforce on its own -- and that is
/// exactly how a macOS-only failure got through: the 104-byte bound is
/// invisible unless something on Linux checks it deliberately. Every fixture
/// here targets the tighter, macOS number instead, so a green Linux CI run
/// can't hide a `UnixListener::bind` that only fails on macOS.
pub const SOCKET_PATH_BUDGET: usize = 104;

/// A tempdir rooted at `/tmp`, for any test that points `HOME` (or otherwise
/// derives a socket path) at it.
///
/// `tempfile::tempdir()` roots at `$TMPDIR`, which on macOS looks like
/// `/var/folders/8j/sfr9qqcj73j4p6nhwcfpr0th0000gn/T/.tmpXXXXXX` -- ~59 bytes
/// on its own, before anything is joined onto it. macOS has no
/// `XDG_RUNTIME_DIR`, so `paths::resolve_from` puts the socket at
/// `<home>/Library/Application Support/nudge/nudge.sock`, another ~45 bytes.
/// Put together that's 104: AT the `SUN_LEN` budget, so `UnixListener::bind`
/// fails with "path must be shorter than `SUN_LEN`" before the test gets to
/// exercise anything.
///
/// Rooting at `/tmp` instead gives a path like `/tmp/nudge-XXXXXX` -- ~21
/// bytes -- for a total of ~66 with the same suffix: comfortably inside the
/// budget. A real macOS user is never at risk either way --
/// `/Users/name/Library/Application Support/nudge/nudge.sock` is ~56 bytes --
/// this is purely a hazard of using a tempdir as `HOME` in a test.
///
/// DO NOT "helpfully" change this back to `tempfile::tempdir()`. On Linux
/// (`SUN_LEN` 108, and often a short `$TMPDIR` already) that revert keeps
/// passing locally and in CI, which is exactly why this got missed the first
/// time -- it only breaks for a macOS user, who is not the one reverting it.
pub fn short_tempdir() -> TempDir {
    tempfile::Builder::new()
        .prefix("nudge-")
        .tempdir_in("/tmp")
        .expect("create a tempdir under /tmp")
}

/// Fails loudly, at fixture-build time, if `socket` would not fit in
/// `sockaddr_un.sun_path` on macOS -- rather than surfacing however many
/// commands later as an opaque `InvalidInput`/EINVAL out of
/// `UnixListener::bind`, deep inside whatever the test was actually trying to
/// check.
///
/// Call this right after building a `Paths` (or any ad hoc socket path) in a
/// fixture that derives it from a tempdir, so a future change to
/// `resolve_from`, a longer prefix, or a longer platform `$TMPDIR` fails here,
/// with a byte count and the path attached, instead of mysteriously.
pub fn assert_socket_path_fits(socket: &Path) {
    let len = socket.as_os_str().len();
    // `display()`, not `{socket:?}`: the Debug form escapes and quotes, which is
    // noise in a message whose whole job is to show you a path you can measure.
    let shown = socket.display();
    assert!(
        len < SOCKET_PATH_BUDGET,
        "socket path is {len} bytes, budget is {SOCKET_PATH_BUDGET} (macOS's \
         SUN_LEN; Linux's is a looser 108, but this suite always targets the \
         tighter number): {shown}\n\
         UnixListener::bind will fail with 'path must be shorter than SUN_LEN' \
         at this length. If this fixture builds HOME from a tempdir, use \
         common::short_tempdir() instead of tempfile::tempdir()."
    );
}

/// A tmux server private to one test, living under its own `TMUX_TMPDIR`.
///
/// `TMUX_TMPDIR` rather than `-L`, because the point is usually a child `nudge`
/// that has to reach this server with no socket flag of its own: give it
/// [`TmuxServer::env`]. `-f /dev/null` keeps the developer's ~/.tmux.conf out,
/// so `base-index` and the like are tmux's defaults.
///
/// Every command carries the env explicitly rather than trusting the process
/// env: a `kill-server` that leaked to the default socket would take down the
/// developer's entire tmux, so it must not be possible for one to.
pub struct TmuxServer {
    tmpdir: std::path::PathBuf,
}

impl TmuxServer {
    /// Start a detached 80x24 session named `session` running `sh`.
    pub fn start(tmpdir: &Path, session: &str) -> Self {
        std::fs::create_dir_all(tmpdir).unwrap();
        let server = TmuxServer {
            tmpdir: tmpdir.to_path_buf(),
        };
        let ok = server
            .tmux()
            .args([
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                session,
                "-x",
                "80",
                "-y",
                "24",
                "sh",
            ])
            .status()
            .expect("spawn tmux")
            .success();
        assert!(ok, "failed to start the private tmux session");
        server
    }

    /// A `tmux` command aimed at this server and nothing else.
    pub fn tmux(&self) -> std::process::Command {
        let mut c = std::process::Command::new("tmux");
        self.env(&mut c);
        c
    }

    /// Run a tmux command on this server and return what it printed.
    pub fn run(&self, args: &[&str]) -> String {
        let out = self.tmux().args(args).output().expect("spawn tmux");
        assert!(out.status.success(), "tmux {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Point `cmd`'s tmux calls at this server. `TMUX` names the socket of the
    /// session running the test, and tmux prefers it, so it goes.
    pub fn env(&self, cmd: &mut std::process::Command) {
        cmd.env("TMUX_TMPDIR", &self.tmpdir)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        // Safe only because of the explicit TMUX_TMPDIR + TMUX removal above:
        // this can address nothing but the server we started in our own tempdir.
        let _ = self.tmux().arg("kill-server").status();
    }
}

/// Whether tmux is installed. Tests that need a real server skip without it.
pub fn tmux_available() -> bool {
    std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}
